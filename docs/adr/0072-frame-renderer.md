# 0072. The frame renderer: one object composes a view's frame, with temporal antialiasing and the upscalers in it

- Status: Accepted
- Date: 2026-10-01
- Builds on: [ADR 0011](0011-render-graph.md) (the render graph),
  [ADR 0043](0043-motion-vectors-and-taau.md) (motion vectors, TAAU),
  [ADR 0047](0047-bloom-and-automatic-exposure.md) (bloom, exposure),
  [ADR 0060](0060-fsr-3-1-upscaling.md) (FSR 3.1, texture LOD bias),
  [ADR 0063](0063-dlss-through-streamline.md) (DLSS, the `Upscaler` interface),
  [ADR 0064](0064-gpu-scene-extraction-runtime.md) (GPU scene, runtime),
  [ADR 0068](0068-skeletal-animation-and-skinning.md) (skinned motion),
  [ADR 0016](0016-viewports.md) (viewports),
  [ADR 0019](0019-networking-model.md)'s addendum (the camera per rendered frame),
  [ADR 0067](0067-no-camera-motion-blur-in-gameplay.md) (cinematic views)
- Research: [docs/research/frame-renderer.md](../research/frame-renderer.md)

## Context

- The engine had every part of a frame and nothing that composed them. TAAU, FSR 3.1
  and DLSS implemented `Upscaler`, the forward renderer had `Temporal` (jitter, motion
  vectors), but they ran only in their own benches on a cube field. The golden test bed,
  `scene_bench` and the benches each wired forward + AO + SSS + atmosphere + shadows +
  bloom + tone mapping by hand; none applied TAA; `engine_runtime` stopped at extraction.
  A showcase Sponza frame had no antialiasing.
- A frame has an order that is easy to get wrong: scene passes at the render size with a
  jittered projection and motion vectors; the temporal resolve or upscale to the output
  size with the reactive mask; bloom, exposure and tone mapping at the output size. Around
  it: the jitter phases and the texture LOD bias the upscaler asks for (set before the
  materials' frame, ADR 0060), history resets on cuts and on size or upscaler changes, a
  dynamic-resolution hook, and per-view state for several views.
- The upscalers depend on `engine_render`; FSR (C++ build) and DLSS (user DLLs) must stay
  optional.

## Options

**Where the composer lives**

1. *In `engine_render`*: cannot reach `engine_upscale`, `engine_fsr` or `engine_dlss`
   (they depend on it). A trait object injected by the caller would work, but every
   caller would still build the upscalers.
2. *In `engine_upscale`*: reaches TAAU and the trait; FSR and DLSS depend on it, so it
   could not construct them.
3. *A new crate `engine_frame` (chosen)* on top of `engine_render` and `engine_upscale`,
   with FSR and DLSS as optional dependencies behind features `fsr` and `dlss` (on by
   default). The upscaler a view uses is a `Box<dyn Upscaler>`: views pay only for the
   one they use. `engine_runtime` and the golden test bed depend on it.

**Shape** (research note: UE's view family / view / view state, Frostbite's frame graph,
HDRP's `HDCamera`, Godot's viewport, Bevy's camera components)

1. *One object per view holding everything*: pipelines duplicated per view.
2. *A static pipeline object configured once*: cannot express per-frame changes (dynamic
   resolution, cuts) or several views without rebuilding.
3. *Shared passes + persistent per-view state + plain per-frame settings (chosen)*:
   `FrameRenderer` (the pipelines: `ForwardRenderer`, bloom, tone map; one per output
   format), `View` (UE's `FSceneViewState`: temporal and occlusion state, upscaler and
   history, adapted exposure, previous color, retired resources), `ViewSettings` (plain
   data, changeable any frame) and `ViewCamera` (the frame's camera with a `cut` flag).
   Two calls per view and frame: `prepare` before `MaterialTable::begin_frame` (the LOD
   bias lives in the samplers), `add_view` in the graph.

**Exposure for FSR and DLSS** (measured, below)

1. *Their automatic exposure* (what the benches did): they judge colors at an exposure the
   player never sees.
2. *Pre-exposed scene color* (UE): every shader's output changes; no measured need for the
   half-float range.
3. *The engine's exposure as a 1 × 1 texture (chosen)*: the multiplier the tone mapper
   applies, fixed (a cleared texture) or automatic. Automatic exposure is measured on the
   scene at render size **before** upscaling, so the upscaler gets this frame's value and
   the histogram costs the render size's pixels. TAAU needs none (it blends reversibly
   tonemapped colors).

**Where screen-space effects plug in**

- One hook, between the scene passes and antialiasing (`add_view_with`), with the scene's
  color, depth, motion output and, when asked (`ViewSettings::previous_color`), last
  frame's antialiased HDR color, copied into a persistent per-view image (zero cost when
  off). This is what screen-space reflections need (HDRP's "before post-process" and its
  history color; UE's previous scene color). **Hi-Z for SSR** is not the occlusion
  culling's pyramid: that one keeps the *farthest* depth per cell (conservative for
  occlusion), SSR's hierarchical trace needs the *closest* (or both); SSR builds its own
  from `depth` in the hook with `hiz.rs`'s reduction.
- **Skinned meshes** (ADR 0068): `SceneInput::skinning` takes the frame's
  `SkinningFrame`; views with motion vectors get the skinned motion pass right after their
  scene passes (each vertex's previous position is kept by the skinning pass, so the
  frame renderer only schedules it).

**Cuts and changes**

- `ViewCamera::cut`: the view's `Temporal`, `Occlusion` and `AutoExposure` reset, the
  upscaler resets, the previous color is invalid for one frame.
- Detected by `prepare`: a new upscaler (or a fallback) or a new output size starts the
  upscaler's history anew (`View::resets` counts both). The motion vectors stay valid.
- An upscaler that is not available (DLSS without Streamline, FSR without the feature)
  falls back to TAAU; `prepare` returns what is used.

**Dynamic resolution**

- *Per-frame scale with targets sized per frame (chosen)*: `ViewSettings::render_scale`;
  the render size follows it, the jitter phases too; the texture LOD bias stays the
  nominal quality's (changing it rewrites every material). Each distinct size is a new
  transient placement in the graph; a GPU-time controller should step coarsely.
- *Fixed max-size targets and a viewport rectangle* (HDRP, UE): no new placements, but
  every pass would take a sub-rectangle. Deferred until a controller exists and churn is
  measured.

**Per-device defaults**

- `DeviceClass::of(device)` (integrated or discrete, from the adapter kind) and
  `RenderDefaults::for_class`: ambient occlusion preset, antialiasing, render size, chosen
  by the measurements below.

## Measurements

ref-1 (`docs/perf/reference-hw.md`), release, `VK_LOADER_LAYERS_DISABLE=~implicit~`,
branch `feat/frame-renderer`. Sponza, 1920 × 1080, through the golden test bed
(`cargo run -p engine_golden --release --example frame_bench`); GPU times are medians over
100 frames; FLIP is the mean against the frame rendered at 3840 × 2160 without
antialiasing and averaged 2 × 2 (AO: against 16 slices × 16 samples).

**Antialiasing and render size** (passes: the GPU's busy time, every pass of the frame;
FLIP: mean against 2 × 2 supersampling, lower is better; the still-camera metric rewards
sharp textures and does not see shimmer in motion, which is what temporal methods remove):

| mode | RTX 4060 Ti passes ms | of which AA | Radeon iGPU passes ms | of which AA | FLIP (RTX / Radeon) |
|---|---:|---:|---:|---:|---:|
| off, 1080p | 0.78 | – | 17.06 | – | 0.0240 / 0.0234 |
| off, 67 % stretched | 0.52 | – | 10.65 | – | 0.0424 / 0.0405 |
| TAA (TAAU at 100 %) | 0.92–1.27 | 0.17–0.20 | 23.15 | 3.85 | 0.0321 / 0.0324 |
| TAAU 67 % | 0.74 | 0.14 | 16.32 | 3.86 | 0.0410 / 0.0415 |
| TAAU 59 % | 0.70 | 0.13 | 14.96 | 3.86 | 0.0441 / 0.0448 |
| TAAU 50 % | 0.67 | 0.13 | 13.81 | 3.86 | 0.0485 / 0.0490 |
| FSR 3.1 Native AA | 1.71 | 0.89 | 42.13 | 20.45 | 0.0333 / 0.0276 |
| FSR 3.1 Quality (67 %) | 1.17–1.43 | 0.56 | 25.31 | 12.54 | 0.0429 / 0.0398 |
| FSR 3.1 Performance (50 %) | 0.99 | 0.46 | 20.79 | 10.78 | 0.0499 / 0.0475 |
| DLAA | 3.11 | 1.78 | – | – | 0.0183 / – |
| DLSS Quality (67 %) | 1.62 | 0.97 | – | – | 0.0231 / – |

- On the iGPU, **TAAU from 67 % is cheaper than no antialiasing at the output size**
  (16.3 against 17.1 ms): the scene passes save more than the resolve's 3.9 ms costs. TAA
  at 100 % costs +6.1 ms there (+36 %), FSR 3.1 is out of reach (12.5–20 ms itself).
- On the RTX, TAA at 100 % costs +0.14 to +0.49 ms (two runs on the shared machine; the
  RTX's clocks vary more than the Radeon's); FSR and DLSS cost 4–9× TAAU's resolve, DLSS
  gives the best image by far.
- The still image favours no antialiasing over TAAU and FSR (their accumulation softens
  textures; DLSS does not): on the cube field of ADR 0060, where edges dominate, TAAU wins
  by 5.3 dB. A temporal-stability metric (shimmer under motion) is a follow-up.
- A texture LOD bias one level sharper than the standard one helps the still image on
  Sponza (TAA 0.0321 → 0.0292, FSR Native AA 0.0333 → 0.0300, TAAU 67 % 0.0410 → 0.0400,
  RTX), unlike ADR 0060's measurement on an upscaled textured ground in motion. The
  bias stays the upscaler's for now (question for the team in the PR).

**Ambient occlusion presets** (mean FLIP against 16 slices × 16 samples per side, same
antialiasing; AO runs at the render size):

| preset | RTX 1080p AO ms | RTX TAAU 67 % AO ms | Radeon 1080p AO ms | Radeon TAAU 67 % AO ms | Radeon TAAU 67 % passes ms | FLIP 1080p / TAAU 67 % |
|---|---:|---:|---:|---:|---:|---:|
| none | – | – | – | – | 16.85 | 0.1461 / 0.1449 |
| Low (1 × 2) | 0.32 | 0.14 | 6.03 | 2.58 | 18.24 | 0.1083 / 0.1032 |
| Medium (2 × 2) | 0.40 | 0.18 | 8.03 | 3.54 | 19.20 | 0.0957 / 0.0938 |
| High (3 × 3) | 0.52 | 0.24 | 12.11 | 5.35 | 21.06 | 0.0703 / 0.0692 |
| Ultra (9 × 3) | 1.10 | 0.49 | 29.15 | 12.87 | 28.00 | 0.0687 / 0.0679 |

(plus the depth prepass AO needs: 0.06 ms RTX, 0.93 ms Radeon at 1080p, 0.72 at 67 %.)
GTAO has no half-resolution mode; rendering at 67 % is the half resolution that exists:
it cuts every preset's cost by 2.3× on the iGPU at the same FLIP.

**Exposure for FSR and DLSS** (PSNR against 2 × 2 supersampling on ADR 0060's cube
field, camera still / circling, `upscale_bench` and `dlss_bench` with
`ENGINE_UPSCALE_EXPOSURE=1`):

| | own automatic exposure | the engine's exposure |
|---|---:|---:|
| FSR 1080p from 720p, RTX | 43.26 / 38.70 dB | 43.56 / 38.89 dB |
| FSR 1080p from 720p, Radeon | 43.20 / 38.67 dB | 43.64 / 38.86 dB |
| FSR Native AA, RTX | 45.54 / 40.82 dB | 46.08 / 41.00 dB |
| DLSS 1080p from 720p, RTX | 45.93 / 39.92 dB | 46.66 / 40.06 dB |
| DLAA, RTX | 47.03 / 41.16 dB | 47.86 / 41.41 dB |

**The composer's own cost**: `prepare` + `add_view` (which includes declaring the scene
passes) take 3.5–10 µs of CPU per view and frame on ref-1 (`game_loop`, 20,000 instances
in a GPU scene, RTX; up to 30 µs while the Radeon window ran beside other agents' work),
TAAU and FSR add a few µs (their passes' setup); 0 heap allocations in
steady state (`engine_frame/tests/zero_alloc.rs`).

**No change where nothing changed**: every existing golden shot (antialiasing off) renders
byte-identical through the frame renderer on both GPUs, and an A/B of the perf gate's four
existing scenes against main, back to back, moves no pass beyond noise (RTX: all within
±0.3 % except `ao denoise` +2.4 % / 1 µs; Radeon: the branch 1–4 % faster, clock noise).

## Decision

- `engine_frame::FrameRenderer` composes every view's frame in this order: scene passes
  at the render size (jitter and motion vectors with temporal antialiasing; the skinned
  motion pass), automatic exposure measured on the scene, the hook for scene effects
  (with last frame's antialiased color on request), antialiasing or upscaling through
  `Box<dyn Upscaler>` with the reactive mask and the engine's exposure for FSR and DLSS,
  then bloom and tone mapping at the output size. `View` keeps each camera's state;
  `prepare` applies `ViewSettings` before the materials' frame (jitter phases, LOD bias,
  creation and retirement of the upscaler and the per-view resources).
- `Antialiasing::Off | Temporal(Taau | Fsr | Dlss)` with a `Quality` render size and a
  per-frame `render_scale`; an unavailable upscaler falls back to TAAU.
- Cuts drop the history (`ViewCamera::cut`, a camera switch or a bumped cut generation in
  the runtime); upscaler or output-size changes do too.
- **Defaults** (`RenderDefaults::for_device`):
  - *discrete*: TAA (TAAU at 100 %) and ambient occlusion High: about 1.8 ms on the RTX
    for Sponza (the two measurements added); TAAU stays the default (FSR is costlier and, on this metric, not better; DLSS
    is better but needs the player's DLLs);
  - *integrated*: TAAU from 67 % and ambient occlusion Low: 18.2 ms on the Radeon against
    28.0 ms for the old defaults (no antialiasing, AO High at 1080p) and 17.1 ms for no
    antialiasing and no AO at all. Low recovers about half of AO's improvement at a
    fifth of High-at-1080p's cost; a game that has the budget raises it (Medium +1.0 ms,
    High +2.8 ms at 67 %).
- The runtime has a `Camera` component (projection, exposure, antialiasing, render size,
  cinematic, priority, active, cut generation) and a `CameraView` that resolves the main
  camera every rendered frame from its fresh `Transform` (ADR 0019's addendum).

## Consequences

- One way to render a frame: the golden test bed (every shot, byte-identical with
  antialiasing off), the runtime's `game_loop` and new code go through `FrameRenderer`.
  `scene_bench` stays hand-wired for now: it lives in `engine_render`, which cannot depend
  on `engine_frame`; moving it to its own crate is a follow-up.
- Views that do not use a feature pay nothing for it: no motion vectors without temporal
  antialiasing (`Off` shots are byte-identical to before and their passes unchanged in the
  perf gate), no exposure texture for TAAU, no previous-color copy unless asked. No shader
  changed (the register rule, CLAUDE.md §5, is unaffected).
- Views sharing a `MaterialTable` share its texture LOD bias (the last `prepare` wins):
  per-view bias would need per-view sampler sets or a bias in the shaders' push constants.
  Revisit with split screen of mixed upscalers.
- Motion blur (ADR 0067) and screen-space reflections plug into the hook; `cinematic` is
  on `ViewSettings` and the runtime's `Camera` already.
- Frame generation (FSR 3 FG, DLSS FG) goes after tone mapping and before UI in this
  composer when it comes; it needs the HUD-less color and the swapchain, beyond this ADR.
