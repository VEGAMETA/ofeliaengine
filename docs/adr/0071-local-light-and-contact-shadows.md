# 0071. Shadows of point and spot lights in a cached atlas, and contact shadows of the sun

- Status: Accepted
- Date: 2026-10-01
- Builds on: [ADR 0044](0044-cascaded-shadow-maps.md) (the sun's cascades),
  [ADR 0045](0045-local-lights.md) (local lights), [ADR 0064](0064-gpu-scene-extraction-runtime.md)
  (the persistent GPU scene and its uploads), [ADR 0065](0065-async-compute.md),
  [ADR 0068](0068-skeletal-animation-and-skinning.md) (skinned meshes),
  [ADR 0062](0062-golden-images-and-perf-gate.md)
- Research: [`docs/research/local-shadows.md`](../research/local-shadows.md)

## Context

Only the sun cast shadows. Point and spot lights lit through walls. A shadow is a depth
render of the scene per view (six for a point light); on the Radeon iGPU a Sponza view costs
~70 µs to draw, so drawing every shadow every frame is out of reach, while most lights and
most of a scene stay still. The cascades also miss contact detail below their texel size.

Constraints: the register rule (forward shading of views that do not use a feature must
not gain registers: on RDNA2 dead code under a specialization constant took it from 64 to
72 VGPRs, 9–13 % slower); zero heap allocations per frame; no patented algorithms.

## Options

**Where shadows live**

1. A shadow map per light (textures, or layers): many descriptors, memory per light, no
   resolution by importance.
2. **One atlas with tiles per light (chosen):** one bindless texture; tile size from the
   light's screen size under a global budget (id Tech, Frostbite, Unity, Godot do this).
3. Virtual shadow maps (UE5): pages where receivers need them; needs meshlet-class
   rendering and page tables. On the roadmap after meshlets; the atlas is the step before.

**Point lights** (research note): cube (6 perspective views, exact, cullable per face),
dual paraboloid (2 views, non-linear projection bends straight edges), tetrahedron (4 views,
clipping per face, uneven texels). **Cube chosen**: exact for any geometry, each caster
culled into 1–3 faces, and the lookup costs a few ALU operations once the face is picked
by the major axis and projected without a matrix. Tetrahedron was not built; it is the
measured follow-up if point light redraws become the bottleneck.

**When shadows are drawn**

1. Every frame: 13.6 ms on the iGPU for 64 lights in Sponza (below). No.
2. **Cached, redrawn on change, within a budget (chosen):** a light's views are drawn
   again only when it changed or an instance in its reach changed; out-of-date shadows
   wait for a per-frame budget of views (oldest and largest first); new ones are drawn at
   once.
3. Static/dynamic split (cached static casters, dynamic ones redrawn over a copy): halves
   redraw cost when only small things move, but needs static/dynamic classification, a
   second tile per light, and a patent review first (Warner Bros, US 11,908,062 B2; see
   Questions). Deferred.

**How shading reads them**

1. Specialization constant in the existing shading: dead code still costs registers on
   AMD. Rejected by the rule.
2. A per-pixel shadow mask computed after the prepass for each shadowed light, read by
   channel: moves the lookup out of the light loop, but needs a prepass, a mask channel
   per light per tile and a mapping light → channel. Kept as the follow-up (see below).
3. **Separate `*Shadowed` entry points (chosen)**, the literal `shadows` argument down to
   the light loop; used only while a shadowed light reaches the view; created lazily with
   the feature. Other views keep their pipelines byte for byte in behaviour and registers.

**Contact shadows:** Bend Studio's wave-based screen-space shadows (Apache-2.0, adapted)
against a per-pixel ray march, both built and measured; **Bend chosen** (1.5× faster on the
RTX, 2.4× on the iGPU, for longer shadows). Read by `*Contact` entry points after the depth
prepass (which they turn on, as AO does).

## Measurements

ref-1 (RTX 4060 Ti; Radeon 2-CU iGPU), release, `VK_LOADER_LAYERS_DISABLE=~implicit~`,
Sponza 1920 × 1080 with the sky; `scene_bench -- --sky --lights N --shadowed-lights N
[--local-shadows-every-frame]`; branch at 19928c4. Lights spread over the scene (ranges
3–8 % of its size).

| | RTX forward | RTX local shadow passes | iGPU forward | iGPU local shadow passes |
|---|---:|---:|---:|---:|
| 4 lights, no shadows | 463 µs | — | 15.47 ms | — |
| 4 shadowed, cached (steady state) | 422 µs | 0 | 17.05 ms (+10 %) | 0 |
| 4 shadowed, all redrawn (12 views) | 422 µs | 22 µs (1.8 µs/view) | 17.11 ms | 0.57 ms (47 µs/view) |
| 16 lights, no shadows | 444 µs | — | 16.45 ms | — |
| 16 shadowed, cached | 445 µs | 0 | 18.43 ms (+12 %) | 0 |
| 16 shadowed, all redrawn (63 views) | 446 µs | 172 µs (2.7 µs/view) | 18.64 ms | 4.71 ms (75 µs/view) |
| 64 lights, no shadows | 453 µs | — | 16.92 ms | — |
| 64 shadowed, cached | 458 µs | 0 | 18.97 ms (+12 %) | 0 |
| 64 shadowed, all redrawn (194 views) | 461 µs | 492 µs (2.5 µs/view) | 19.22 ms | 13.57 ms (70 µs/view) |

- **Steady state with everything cached costs no pass at all;** the CPU's planning adds
  4–16 µs to `add_passes` (4–64 lights).
- **The budget** (12 views a frame by default) caps redraws at ~30 µs (RTX) and ~0.9 ms
  (iGPU) a frame while lights or casters move.
- **Heavy case** (perf gate scene `sponza_local_shadows`: 7 lights with 8–12 m ranges, every
  pixel lit by most of them): forward 642 → 830 µs (RTX), 23.4 → 34.3 ms (iGPU). Of the
  iGPU's 10.9 ms: ~2.8 ms the shadowed pipeline's registers (85/90 VGPRs against 64/80),
  ~2.4 ms the three extra filter taps, the rest the lookups' arithmetic and latency.

**Contact shadows** (`scene_bench -- --sky --prepass --contact`):

| | RTX | iGPU |
|---|---:|---:|
| Bend pass | 115 µs (graphics queue; 182 µs read when overlapped on the async queue, frame +89 µs) | 2.62 ms |
| forward with the mask | 298 → 303 µs | 8.66 → 9.52 ms (+10 %) |
| ray march (16 samples), dropped | 172 µs | 6.36 ms |

**Registers** (`ENGINE_PIPELINE_STATS=1 ENGINE_ADAPTER=radeon scene_bench --sky
--pipeline-stats`): the existing variants are unchanged at 48/64/64/80 (plain, lights,
environment, both), 35/78/91 (atmosphere pass, transparent under atmosphere without and
with lights). New: shadowed 85/90 (lights, lights + environment); contact 47/64/72/80.

**Perf gate:** no existing scene's pass got slower (A/B against main back to back: RTX
equal or faster; iGPU within the machine's load noise, see the PR). New scene
`sponza_local_shadows`.

## Decision

- `forward::LocalLight::shadow` marks a light; `ForwardRenderer::set_local_shadows(Some(
  LocalShadowSettings))` turns the feature on (atlas 4096² D16, tiles 64–1024, 0.5 texels per
  pixel, 12 views a frame). `local_shadows.rs` holds the allocator, the cache and the
  planning; `forward.rs` the passes; `forward.slang` the culling, drawing and lookup.
- The GPU scene records the boxes (before and after) of every instance its upload touched;
  `GeometryPool::mark_deformed` (called by `Skinning::update_bounds` for re-posed skinned
  meshes) changes a mesh's revision, which the draw-list comparison and the scene's refresh
  both see.
- `ForwardRenderer::set_contact_shadows(Some(ContactShadowSettings))` turns on the sun's
  contact shadows (off by default: 2.6 ms on the iGPU).
- Read-only depth attachments now use `STORE_OP_NONE` (a store is a write, which raced a
  compute pass sampling the depth between the prepass and the shading).

## Consequences

- A game marks the lights that need shadows; still lights in a still scene cost only the
  shading's lookups. Moving lights spread over frames at the budget, with one frame of lag
  per light beyond it.
- Views of one renderer share the atlas: give them the same light list (the cache is keyed
  by the light's index).
- Shading with shadowed lights costs +10–12 % forward on the iGPU with typical lights, up
  to +46 % when every pixel sees many large shadowed lights. Revisit with the per-pixel
  shadow mask (option 2) when a game needs many large shadowed lights on the iGPU.
- Transparent surfaces get local shadows (their own entry points) but not contact shadows
  (they are not in the depth buffer); subsurface materials get local shadows, not contact.

## Questions for the team

- Static/dynamic split of cached shadows: Warner Bros' US 11,908,062 B2 and US 12,315,068 B2
  claim combining static and dynamic shadow maps per texel through a dynamic mask. The
  general "copy static depth, draw dynamic casters over it" (UE4 4.9, 2015) looks like prior
  art, but a review is needed before we build either.

## Addendum (2026-10-11): deterministic contact shadows

Bend's layout writes many pixels more than once (its lines converge towards the light:
4–38 % of a 320 × 180 target's pixels, in the CPU model of `contact.rs`), and the writers
disagree at depth edges, so with plain writes a few pixels changed from run to run, and
from frame to frame for a still camera (5–11 pixels, up to 25 levels, both GPUs). The
owner chose to try making it deterministic: the mask is now `R32_UINT`, filled with 1 by
a clear pass, and every line writes with an atomic minimum of its result's bits (values
0..1 order as their bits do), so the darkest result stays whatever the order; the shading
reads the bits back as a float. Renders are the same bytes run after run on both GPUs.

Cost, `scene_bench --contact` (Sponza, 1080p): the RTX's contact passes 182–198 → 210 µs;
the Radeon's 2,946 → 3,009 µs and its forward pass +180 µs (it reads 4 bytes of mask per
pixel instead of 1), about 2 % of its frame. Only views with contact shadows pay it.

## Addendum (2026-10-15): moved casters redraw pieces of the views

Queue item 6b. The cascades already draw only what changed (ADR 0093's step 2). A light's
views were drawn whole when any caster in its range changed: with things moving near
lights, 12 views a frame cost the Radeon about 1.6 ms in Sponza.

- **What a light keeps.** An out-of-date light keeps the world boxes of what changed in its
  range, old and new places: up to 8, more merged into the box that grows least.
- **Pieces.** For each of its views, the boxes in the view's frustum are projected to
  rectangles of its tile (a texel wider), gathered into at most 2.
  - A view no box reaches is not drawn.
  - A view is drawn whole when its pieces would cover more than half of it, or a box in
    its frustum reaches across its eye plane (around the light).
- **Drawn through the view's projection narrowed to the piece,** with the piece as viewport
  and scissor. Each piece is a drawn view of its own, so nothing else changed:
  - every texel lands where the view puts it;
  - the GPU culling (the frustum's planes from the matrix) keeps what reaches the piece;
  - the level of detail stays the view's.
- **The budget counts a piece as a view.** A large mesh reaching into a piece is drawn
  whole, its vertices as for the view.
- **The draw lists are sized at least for the budget,** so their sizes stay put while
  pieces change the count every frame (no allocation per frame).

Measured (ref-1, release, `scene_bench --sky --lights 16 --shadowed-lights 16`; the movers
are the scene's smallest draws, going back and forth). Means over 400 frames; `budget
200` redraws every out-of-date light every frame, the same work before and after:

| scene, movers, budget | Radeon before → after | RTX before → after | lights updated a frame |
|---|---|---|---|
| Intel's Sponza, 256, 12 | 4.52 → 2.78 ms | 140 → 91 µs | 2 → 4 |
| Intel's Sponza, 256, 200 | 10.35 → 3.23 ms | 311 → 104 µs | 5 → 5 |
| Sponza, 64, 12 | 1.61 → 1.83 ms | 56 → 67 µs | 2 → 3 |
| Sponza, 64, 200 | 2.82 → 2.02 ms | 92 → 69 µs | 3.5 → 3.5 |

- **For the same work, 25–69 % less.** Intel's Sponza is many meshes, so a piece draws
  only those near the movers. Sponza is a few large ones, each drawn whole into any piece
  it reaches.
- **At the default budget, more lights are brought up to date a frame,** since views no
  change reaches cost nothing:
  - Intel's Sponza: twice as many lights, in 35–39 % less time;
  - Sponza: half as many again, in 13–20 % more time (each light 24 % cheaper).
- **Tests:**
  - `local_shadows_match_analytic_shadows_and_cache`, both GPUs:
    - a caster moved near the spot light draws one piece;
    - moved back, the frame is the reference's bit for bit (the narrowed projection draws
      as the view);
    - the pole beside the point light draws 2 pieces instead of 6 views;
    - a removal draws one piece.
  - Unit tests of the pieces, the narrowed projection, the merging and the budget.
  - No allocation per frame.
- **Static scenes** (the perf gate's) draw nothing either way.
