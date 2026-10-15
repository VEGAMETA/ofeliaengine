# 0073. Local reflections: reflection probes and screen-space reflections

- Status: Accepted
- Date: 2026-10-02
- Builds on: [ADR 0046](0046-image-based-lighting.md) (the sky's prefiltered cube, split
  sum, specular occlusion), [ADR 0069](0069-sky-atmosphere-and-fog.md) (the atmosphere's
  full-screen pass), [ADR 0058](0058-ambient-occlusion.md) (GTAO),
  [ADR 0064](0064-gpu-scene-extraction-runtime.md) (the GPU scene's change tracking),
  [ADR 0072](0072-frame-renderer.md) (the frame renderer), [ADR 0035](0035-occlusion-culling.md)
  (the depth pyramid)
- Research: [`docs/research/reflections.md`](../research/reflections.md)

## Context

- Specular light came only from the sky's prefiltered cube. Inside buildings every glossy
  surface reflected the bright sky (Sponza's arcades, floors); nothing reflected nearby
  objects. The owner asked for screen-space reflections and "very fast reflections".
- Hard rule: views that do not use a feature pay nothing for it, not even registers
  (opaque shading is at 48/64/64/80 VGPRs on the Radeon iGPU; 72 cost 9–13 %).
- No patented algorithms, permissive licenses only. Patents found and avoided are listed
  in the research note (SSR with a ray-traced fallback: EA, Microsoft).

## Options

**Where probes' light is applied** (Sponza 1080p, 6 probes, Radeon iGPU; RTX below):

| | VGPRs | forward | extra pass |
|---|---:|---:|---:|
| sky only (environment variant, prepass) | 64 | 8.7 ms | – |
| A. probe lookup in the forward shading (entry points of their own) | 87 | 16.8 ms | – |
| **B. one full-screen composite after the opaque surfaces (chosen)** | 72 forward / 55 composite | 8.9 ms | 4.5–4.9 ms |

A raises the whole forward pass's registers (occupancy 8 → 5 waves); B pays one
full-screen pass whose floor (depth, normal, weight reads and the RGBA16F blend) is
2.0 ms on the iGPU and 60 µs on the RTX, but it is the same pass SSR needs, and the
forward shading of views with reflections only adds two render targets (72 VGPRs, +0.2
ms). Reading probes through a `Probe*` in Slang loaded the whole 144-byte struct: read
float4 by float4, the composite went from 77 to 43 VGPRs (before the search loop).

**How the composite swaps the light:** the reflection shading writes the normal and
roughness (A2B10G10R10) and the weight of the prefiltered radiance (split sum × specular
occlusion, RGBA8) and keeps lighting with the sky as before; the composite adds
`weight × (reflection − sky)`, recomputing the sky's sample exactly. So the image SSR
traces holds every surface's own (sky) reflection, and nothing else changes.

**Where the composite runs: in the scene passes, or in the frame renderer's hook**
(ADR 0072 offers a hook after the scene passes, with last frame's color):

- The hook comes after the atmosphere's pass (fog, aerial perspective) and the transparent
  surfaces. A reflection added there is not fogged and lands on glass. Measured on the
  procedural scene with dense height fog and a mirror floor (RTX, same frame, composite
  moved to the hook's position): the floor's reflections punch through the fog with their
  full brightness and the glass cube gets the floor's reflection on top; in the scene
  passes they fog like everything else.
- SSR traces the current frame's lit opaque image there: exact, no reprojection or
  ghosting, no previous color needed. Last frame's color (multiple bounces, transparency
  in reflections) is a quality option later, through the same composite.
- **Chosen: in the scene passes** (`ForwardView::reflections`), after the opaque and
  subsurface passes, before the atmosphere and the transparent surfaces. The frame
  renderer wires it (`ViewSettings::screen_space_reflections`, the `View` owns the
  history; `SceneInput::reflection_probes`), and the hook gets the inputs and the result
  (`ForwardPasses::reflections`) for effects of its own.

**Probe capture:** re-rendered by the forward renderer (same materials, lights, shadows;
captures cached) rather than G-buffer probes relit every frame (5–6 × the memory,
approximate shadows; the follow-up if time of day forces constant captures). Six views
with full cascades: 1.1 ms (RTX) / 8.3 ms (Radeon) per probe; **one shadow cascade fit
to the sphere the probe sees, shared by its faces: 0.18 / 2.4 ms**.

**Probe selection:** a tile pass setting bit masks (as local lights) cost a pass for
nothing at these counts; **chosen:** bit masks per 16 × 16 screen cell and per 64
view-depth z-bins, computed on the CPU into the view's constants, smallest probes first,
the first two whose influence reaches a pixel blended front to back.

**SSR:** linear marching (misses thin objects, fixed cost) vs **Hi-Z traversal (chosen,
FidelityFX SSSR's, MIT)** against a nearest-depth pyramid of its own (the occlusion
culling pyramid keeps the farthest depth); cone tracing vs **stochastic VNDF rays with a
spatial and a temporal filter (chosen)**; full vs **half resolution (default)**.

## Decision

- **`engine_render::reflections`:**
  - `ReflectionProbes` (`ProbeSettings`: capacity, resolution 128, faces per frame 6, near
    plane): `add`, `set`, `remove`, `refresh`, `refresh_all`, `invalidate_region`,
    `stats`. A `ReflectionProbe` has a capture position, an `influence` box (with
    `blend_distance`), a `parallax` box (rotatable `ProbeBox`es), intensity, far plane and
    `ProbeRefresh::{OnChange, Manual}`; `ReflectionProbe::room` fits a room.
  - Captures: the forward renderer draws the faces (passes named `probe …`, no ambient
    occlusion, no atmosphere pass: the sky comes from the environment's cube in the face
    copy, the sun from the directional light), `probeFace` copies them into a capture cube
    (texel by texel through the face camera), `probeDownsample` and `probePrefilter` (64
    GGX samples, filtered importance sampling) fill the probe's layers of an RGBA16F cube
    array (6 mips, as the sky's). Captured again on edits, `refresh`, a changed sun or sky
    (`EnvironmentFrame::rebuilt`), and on the GPU-scene path when instances inside a
    probe's boxes change (`SceneFrame::changed_boxes`); the nearest waiting probe first,
    `faces_per_frame` at most.
  - `ScreenSpaceReflections` (one per view, at its size; `SsrSettings`: half resolution,
    max roughness 0.6, 64 steps, thickness 0.3, temporal 0.15): `ssrDepth` (nearest-depth
    pyramid, all levels, in a buffer), `ssrTrace` (SSSR traversal with its low-occupancy
    exit; VNDF rays above roughness 0.1; validation: on screen, not the sky, front faces,
    within the thickness; confidence fades at the screen's edges and towards the
    roughness cutoff), `ssrSpatial` (at the trace's resolution, group shared memory,
    depth/normal/roughness weights), `ssrTemporal` (upsamples by depth, reprojects through
    the reflected point, clips history to mean ± 1.25 σ).
  - The composite: three entry points (probes, SSR, both), additive blending; the SSR-only
    one skips every pixel without a hit.
- **`ForwardRenderer::set_reflections`** makes the reflection shading (16 entry points
  `fragmentReflect*`: with/without motion vectors, ambient occlusion, the sun's contact
  shadows and local lights' shadows, ADR 0071; 24 pipelines with the local-light variants)
  and the passes lazily; **`ForwardView::reflections`** (`Reflections::NONE` elsewhere).
  Views with reflections run the depth prepass. Specular occlusion (Lagarde 2014) from the
  material's occlusion and GTAO's is in the weight, so probes and SSR are occluded alike.
- **The frame renderer:** `ViewSettings::screen_space_reflections`, `SceneInput::reflection_probes`,
  history retired on size changes, reset on cuts.
- `Image` gained cube arrays; the device enables `imageCubeArray` (every desktop GPU).

## Measurements

ref-1, release, `VK_LOADER_LAYERS_DISABLE=~implicit~`, Sponza 1920 × 1080 with the sky
(`scene_bench --sky`), 6 probes (`--probes 3,1,2`, 128²), GPU µs, frame = GPU frame end to
end (the machine was shared with other agents; RTX numbers vary ±10 %).

| | RTX 4060 Ti | Radeon iGPU |
|---|---:|---:|
| frame without reflections (prepass on) | 576 | 12,030 |
| frame with 6 cached probes | 694 | 16,750 |
| probe composite (cached probes) | 138 | 4,500 |
| capture of a whole probe: views + copy / mips + prefilter | 183 / 47 | 2,430 / 660 |
| one face per frame (its own shadow cascade) | 70–86 | 950–1,070 |
| frame with SSR, half resolution | 838 | 19,510 |
| SSR half: depth pyramid / trace / spatial / temporal / composite | 29 / 119 / 55 / 57 / 36 | 367 / 2,541 / 1,489 / 1,671 / 939 |
| frame with SSR, full resolution | 1,227 | 29,870 |
| SSR full: same passes | 27 / 352 / 196 / 61 / 33 | 369 / 8,618 / 5,974 / 1,721 / 958 |
| frame with probes + SSR half (composite) | 947 (141) | 23,590 (5,047) |

- **Registers** (Radeon, `reflection_registers` example; `scene_bench --pipeline-stats`):
  opaque shading unchanged at 48/64/64/80; reflection shading 72 (88 with local lights);
  composite 55 (probes), 40 (SSR), 57 (both).
- **Every existing golden image is byte-identical on both GPUs** (the shared forward
  shader's SPIR-V changed only in constant ids for three motion entry points).
- **CPU:** the reflection constants and probe lists take a few µs per view; a capture
  frame lays the draws out once for its faces.

**Correctness** (`engine_render/tests/reflections.rs`, synchronization validation, both GPUs):

- Parallax: a mirror floor in a room of colored glowing walls, a probe captured off-centre:
  11,141 of 11,152 floor pixels reflect the wall the analytic ray–box intersection names
  (the plain reflected direction would read the wrong wall for 3,968).
- Energy: white metals of roughness 0–1 inside an evenly glowing room reflect 2.000
  (≤ 0.001 off) at every probe mip.
- Caching: two probes, two faces a frame → faces drawn 2,2,2,2,2,2,0,0,0,0; a refresh
  recaptures one probe only; a change far away recaptures nothing, one inside a box that
  probe; on the GPU-scene path, moving an instance inside a probe recaptures it, a static
  scene captures nothing.
- SSR: a mirror floor before a glowing block: every floor pixel whose reflected ray meets
  the block's visible face shows its color, every other one shows nothing, at full and
  half resolution (1,196 / 1,196 and 12,803 / 12,803).
- Zero heap allocations per frame (`reflections_zero_alloc.rs`): capturing every frame,
  and with cached probes. A capture every few frames allocates in the render graph (it
  frees transient images a frame did not use and makes them again): a graph property,
  noted below.
- Golden images (both GPUs): `procedural_reflections` (mirror-like metal floor, SSR and a
  probe), `sponza_probes`. Perf gate scene `sponza_reflections` (6 cached probes, SSR at
  half resolution).

## Consequences

- Interiors reflect their rooms, floors reflect what stands on them; views without
  reflections are unchanged (shading, registers, images, passes).
- **Cost on the iGPU is high:** 4.5 ms for probes and 7.5 ms for SSR at half resolution at
  1080p (+37 % and +62 % of the frame), mostly memory traffic of full-screen passes.
  Under TAAU from 67 % (ADR 0072) they scale with the render size. `RenderDefaults` keeps
  reflections off by default; turning them on for integrated GPUs is a product decision.
- Transparent surfaces keep the sky's reflections; reflections in fog are fogged.
- Views with reflections keep local and contact shadows (tested: identical images with SSR
  on and off on a rough floor under a shadowed spot light); probe captures draw without
  ambient occlusion, contact or local-light shadows (their cubes are blurred anyway).
- **Not done:** probes relit from G-buffers; probe shadow maps cached across frames (one
  face per frame draws its cascade each frame); probes or SSR on transparent surfaces;
  SSR ray reuse across neighbours (Stachowiak) and last frame's color for multiple
  bounces; GTSO (needs bent normals from GTAO); ~~the render graph keeping transient
  objects of frames that skip a capture (intermittent captures allocate)~~ (done, see
  the addendum).

## Addendum (2026-10-15): the capture order, an interval for the scene's changes, and the graph's frame shapes

Probes placed as entities in a game world (ADR 0099 step 4) met moving things for the
first time: `flight` with four probes along Sponza's courtyard, 2,000 spinning cubes and
64 walking characters inside them.

- **A probe was captured in every frame.** Instances moving inside its boxes asked for a
  new capture as soon as one finished.
- **The farthest probe was never captured:** the waiting probe nearest the camera always
  went first (3 of 4 captured after 600 frames).

Changes:

- **The probe captured longest ago goes first** (one never captured first of all), the
  nearest among equals. Every waiting probe gets its turn; at load, all equal, the order
  is the old one.
- **`ProbeSettings::scene_interval`, 30 frames by default.** A probe that only the scene's
  changes ask for (instances moving, appearing or going inside its boxes;
  `invalidate_region`) waits until that many frames have passed since its last capture
  started. Edits of the probe, `refresh` and a change of the sky or the sun capture at
  once. 0 is the old behavior.
- **The render graph keeps the shapes of recent frames** (ADR 0011's addendum). A frame
  that captures has transients of its own, so its placement differs from the frames
  around it. Every capture re-placed the transients and made their objects again twice,
  in and out. A slot now keeps up to 4 placements per group and keeps objects no frame
  used for 128 of its frames. Objects alias the same heap memory, and a transient starts
  undefined in every frame, so a kept object is as good as a new one.
  - Test (`zero_alloc/graph.rs`, every seventh frame with a pass and a transient of its
    own): main's graph made 15 objects and 15 heap allocations in 100 frames; now none.

Measured on ref-1 (`flight`, release, 1797 × 1011 window, latency control off; RTX 1,800
frames, Radeon 900):

| RTX 4060 Ti | GPU frame median µs | CPU frame median / p99 µs | graph compile + record p99 µs | captures |
|---|---:|---:|---:|---:|
| no probes | 1,782 | 851 / 1,091 | 256 | – |
| four probes, captured whenever asked (interval 0) | 2,191 | 1,039 / 1,319 | 441 | every frame |
| interval 30, the graph as before | 1,911 | 909 / 2,307 | 1,530 | 224 of 1,680 frames |
| interval 30 | 1,914 | 913 / 1,187 | 380 | 224 of 1,680 frames |

- **Radeon iGPU, GPU frame median:** no probes 21.3 ms; interval 0 28.1 ms; interval 30
  24.7 ms. Its p99 (33.5 ms) is a capture's six faces in one frame, about 6 ms there; one
  face a frame (`faces_per_frame`) would spread it, a default per device class to
  measure.
- **The composite costs every frame either way:** 93 µs on the RTX, 1.56 ms on the
  Radeon.
- Golden images are unchanged: their scenes are still, so their probes capture at load
  only.
