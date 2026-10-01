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

MEASUREMENTS

## Decision

DECISION

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
