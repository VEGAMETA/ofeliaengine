# The frame renderer: composing a view's frame (scene passes, temporal upscaling, post)

Checked 2026-10-01. Sources at the end. No code was copied: the engines below were read
for their structure (UE's source is under the Unreal Engine EULA and was read through its
public documentation and talks only; Godot and Bevy are MIT, Unity's HDRP package is under
the Unity Companion License and was read as documentation).

## Why

- The engine had the parts of a frame (the forward renderer with its culling, shadows,
  ambient occlusion, subsurface scattering, sky, transparency; TAAU, FSR 3.1 and DLSS
  behind one `Upscaler` interface; bloom, automatic exposure, tone mapping) and nothing that
  put them together. The golden test bed, `scene_bench` and every benchmark wired them by
  hand, none applied temporal antialiasing, and the game runtime (ADR 0064) stopped at
  extraction. A game could not ship like that; a showcase Sponza frame had no
  antialiasing.
- What must be composed, in order:
  1. the scene at the **render** size, with a jittered projection and motion vectors when
     a temporal method resolves it;
  2. the temporal resolve or upscale to the **output** size, with the reactive mask of the
     blended passes, the camera's planes and the frame time;
  3. bloom, exposure and tone mapping at the output size.
- And around it, per view and across frames: the jitter sequence and texture LOD bias the
  upscaler asks for, history resets on camera cuts and on size or upscaler changes, a
  render-scale hook for dynamic resolution, and state that survives frames (history,
  exposure, occlusion, last frame's camera) for each of several views.

## How others do it

### Unreal Engine 5: view family, view, view state, scene renderer

- **`FSceneViewFamily`**: the views rendered together into one target this frame (split
  screen players, stereo eyes), with what they share: the time, the show flags, the
  screen-percentage interface (`ISceneViewFamilyScreenPercentage`) and the temporal
  upscaler (`ITemporalUpscaler`, set by a view extension such as DLSS's or FSR's plugin).
- **`FSceneView`**: one frame's camera: view and projection matrices, the temporal jitter
  in pixels (`TemporalJitterPixels`), the view rectangle at render resolution and the
  unscaled one at output resolution, `bCameraCut`.
- **`FSceneViewState`**: what one view keeps across frames: TAA/TSR history, the eye
  adaptation (exposure) buffers, occlusion query history, last frame's view matrices
  (`PrevFrameViewInfo`), the jitter index. Created per player/camera and owned by the game
  (`ULocalPlayer::ViewState`).
- **`FSceneRenderer::Render`** (the deferred or mobile renderer) declares the whole frame
  into the Render Dependency Graph: scene passes at the view's render rectangle, then the
  post-processing chain, where the temporal upscaler is one pass
  (`ITemporalUpscaler::AddPasses`) that brings color to the output size; everything after
  it (bloom, tone mapping) runs at output resolution.
- **Mip bias** follows the screen percentage (`log2(render / output)` plus a user
  offset), passed to materials through the view's uniform buffer.
- **Exposure**: UE pre-exposes scene color by last frame's exposure (`PreExposure`) to keep
  HDR values in half-float range, and hands upscalers that exposure.
- Takeaway: three objects, not one: frame-wide settings, per-frame view, persistent view
  state. The upscaler is a pass in the middle of post-processing, with a screen
  percentage before it and output resolution after it.

### Frostbite: the frame graph

- O'Donnell (GDC 2017): the renderer is a set of features that each **declare** their
  passes and resources into a frame graph every frame (setup), which is compiled (culling,
  lifetimes, aliasing) and executed. Features are composed by code that runs every frame,
  not by a static pipeline object.
- Takeaway: the composer is just ordinary code that declares passes into the graph in the
  right order; the engine's `RenderGraph` already does the compile and execute steps.

### Unity HDRP: `HDCamera`, custom pass injection points, dynamic resolution

- **`HDCamera`** holds a camera's persistent data: the history buffers (`RTHandle`
  history system, `GetPreviousFrameRT` for the previous color, depth and motion), the TAA
  jitter, the exposure textures, the frame count, and a "reset" flag for cuts.
- **Injection points** let user passes run at fixed places in the frame: before rendering,
  after opaque depth and normals, before pre-refraction, before transparents, before
  post-processing, after post-processing.
- **`DynamicResolutionHandler`** sets a screen percentage per frame; render targets are
  allocated at the maximum size and rendered into a sub-rectangle, and the upscale filter
  (TAAU, FSR 1, DLSS through its plugin) runs before post-processing.
- Takeaway: a per-camera state object with history, plus well-defined hook points; dynamic
  resolution as a per-frame scale with a fixed maximum.

### Godot 4 and Bevy

- **Godot**: a `Viewport` has `scaling_3d_mode` (bilinear, FSR 1, FSR 2), `scaling_3d_scale`,
  `use_taa` and `texture_mipmap_bias` (set automatically with FSR 2); its
  `RenderSceneBuffers` keep per-viewport history. The renderer composes the frame in
  `RendererSceneRenderRD::render_scene`.
- **Bevy**: a camera entity carries components that turn features on (`TemporalJitter`,
  `MipBias`, `TemporalAntiAliasing`, the motion-vector prepass, DLSS); each camera gets its
  view's sub-graph of the render graph, and per-view state lives in components on the
  render-world view entity.
- Takeaway: antialiasing, render scale and mip bias are per-view settings; the camera is
  an entity in the scene that the renderer reads every frame.

## Exposure and the vendor upscalers

- **FSR 3.1** takes either an exposure texture (1 × 1, the value the application will
  apply) and a pre-exposure (the factor already applied to the input color, divided out),
  or `FFX_FSR3UPSCALER_ENABLE_AUTO_EXPOSURE` to compute its own. Its heuristics (luma
  instability, locks, the internal tonemap of colors) run on exposed colors.
- **DLSS** (Programming Guide §3.10): an exposure texture (1 × 1) or the auto-exposure
  flag; NVIDIA recommends passing the game's exposure when it has one, auto-exposure
  otherwise.
- The engine does not pre-expose scene color (exposure is applied at tone mapping), so
  pre-exposure stays 1. Its exposure is either fixed or measured by `AutoExposure`; both
  can be given to FSR and DLSS as a 1 × 1 texture: so they see the colors as the player
  does. TAAU blends reversibly tonemapped colors (`c / (1 + max c)`) and needs none.
- Where to measure automatic exposure: on the antialiased image at output size (after the
  upscaler; the upscaler then gets last frame's, as UE's pre-exposure does) or on the scene
  at render size before it (this frame's, cheaper by the render scale, jitter noise
  averaged out by a 256-bin histogram of every second pixel). The engine takes the second.

## What we take

- **Two objects and a settings value**, after UE's split: `FrameRenderer` (the passes'
  pipelines, shared by every view; UE's scene renderer), `View` (persistent per-view state;
  UE's `FSceneViewState`, HDRP's `HDCamera`), `ViewSettings` (per frame, plain data; UE's
  view family options, Godot's viewport properties). The per-frame camera is a small value
  (`ViewCamera`, UE's `FSceneView` reduced to what the passes need) with an explicit `cut`
  flag (UE's `bCameraCut`, HDRP's reset).
- **Declared every frame into the existing render graph** (Frostbite): no new scheduling,
  the graph orders, aliases and synchronizes.
- **The upscaler as one pass between scene and post** (UE, HDRP), through the existing
  `Upscaler` trait object, so FSR and DLSS stay optional crates behind features.
- **One hook point** between the scene passes and antialiasing, with the scene's targets,
  the motion vectors and last frame's antialiased color (HDRP's "before post-process", and
  its history system's previous color): screen-space reflections and other scene effects
  go there. More points (before transparents) can come with the features that need them.
- **The camera as a component** (Bevy) read every rendered frame from its fresh local
  transform (ADR 0019's addendum), with antialiasing, render size, exposure and the
  cinematic flag (ADR 0067) on it.
- **Dynamic resolution as a per-frame scale** (HDRP, UE) with the LOD bias kept at the
  nominal quality's: the bias lives in the samplers, and changing it rewrites every
  material (ADR 0060).

## What we don't take (now)

- **A view family object.** Split screen and render targets are several `View`s rendered
  in one graph; what UE's family shares (time, show flags) is the caller's.
- **Sub-rectangle rendering for dynamic resolution** (fixed maximum-size targets, a
  viewport rectangle). The graph's transient textures are sized per frame; a size change
  costs a new placement, not a reallocation once the heap is large enough. Revisit when a
  GPU-time controller exists and measurements show churn.
- **Pre-exposed scene color.** Half-float range has not been a problem with the engine's
  exposures; it would change every shader's output and every golden image.

## Sources

- Unreal Engine 5 documentation: "Screen Percentage with Temporal Upscale", "Temporal
  Super Resolution", the `FSceneViewFamily`, `FSceneView`, `FSceneViewState`,
  `ITemporalUpscaler` API reference (docs.unrealengine.com, checked 2026-10-01).
- Y. O'Donnell, "FrameGraph: Extensible Rendering Architecture in Frostbite", GDC 2017.
- Unity HDRP documentation: "Custom Pass" (injection points), "Dynamic resolution",
  "Deep learning super sampling (DLSS)", `HDCamera` (docs.unity3d.com, HDRP 17, checked
  2026-10-01).
- Godot 4 class reference: `Viewport` (`scaling_3d_mode`, `scaling_3d_scale`,
  `texture_mipmap_bias`, `use_taa`), docs.godotengine.org, checked 2026-10-01 (MIT).
- Bevy `bevy_render::camera` (`TemporalJitter`, `MipBias`), github.com/bevyengine/bevy,
  main, checked 2026-10-01 (MIT OR Apache-2.0).
- AMD FidelityFX SDK 1.1.4, FSR 3.1 upscaler documentation: exposure, pre-exposure,
  `FFX_FSR3UPSCALER_ENABLE_AUTO_EXPOSURE` (MIT).
- NVIDIA DLSS Super Resolution Programming Guide (310.x), §3.10 exposure; Streamline 2.x
  `sl_dlss.h` (`useAutoExposure`), `sl_core_types.h` (`kBufferTypeExposure`) (MIT).
