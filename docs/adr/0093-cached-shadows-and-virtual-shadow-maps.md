# 0093. Cached shadows and virtual shadow maps: scrolling cached cascades first

- Status: Accepted (option 2 first, then option 1's pages; one cache: the owner, 2026-10-14)
- Date: 2026-10-14
- Research: [docs/research/virtual-shadow-maps.md](../research/virtual-shadow-maps.md)
- Related:
  - [ADR 0044](0044-cascaded-shadow-maps.md): cascades, texel snapping, map-based selection, dithered transitions
  - [ADR 0071](0071-local-light-and-contact-shadows.md): the static/dynamic patent question
  - [ADR 0085](0085-visibility-buffer-and-meshlets.md): meshlet culling of the cascades
  - queue item 5

## Context

- **The sun's shadows are drawn from scratch every frame.** Each of 4 cascades of 1024²
  redraws every caster over its square. On a dense scene they are the frame's largest
  cost (each device class's defaults, the GPU frame):
  - Intel's Sponza: 50 % of the RTX's frame (1,079 of 2,152 µs) and 37 % of the Radeon's
    (21.3 of 57.9 ms);
  - Sponza: about 13 % on the RTX and 10 % on the Radeon.
- **Almost none of that work changes from frame to frame.** The camera moves the cascades
  by whole texels, and casters seldom move.
- **The cost grows with each cascade.** Each one redraws much of the scene. Intel's Sponza
  on the Radeon takes 9.9, 12.8, 18.1 and 22.1 ms with 1–4 cascades.
- **Virtual shadow maps** (queue item 5) were kept by ADR 0044 for after meshlets. They
  cache pages and add resolution where pixels need it. The research note compares them
  with simpler caching.

## Options

1. **Virtual shadow maps** (Unreal 5's design, StratusGFX, GPU Zen 3):
   - **What it is:** a clipmap of virtual maps for the sun, 128² pages backed from one
     physical pool only where the depth buffer's pixels sample, kept across frames until
     invalidated.
   - **Rendering:** dirty pages only, through the meshlet cull. Shading goes through a
     page table.
   - **Pros:** it saves the cost and gives the sharpest shadows: texel density matched to
     the pixels at any distance. Local lights fit it later (Olsson et al.).
   - **Cons:** the most to build:
     - page marking from the depth buffer;
     - a GPU page allocator and a page table;
     - rendering into scattered pages (per-page viewports or atomic depth);
     - filtering that knows page borders;
     - pool overflow handling.
     
     Unreal calls it a poor fit without Nanite-class culling (we have meshlets, not
     Nanite).
2. **Scrolling cached cascades:**
   - **What it is:** the cascades stay as they are, but keep their depth across frames,
     each a layer of an array texture addressed toroidally (wrap-around, so hardware PCF
     needs no border handling).
   - **The light's depth range** is fixed by the scene's bounds instead of each cascade's
     fit, so cached depths stay valid.
   - **Each frame a cascade draws only what changed:**
     - the strips the camera's snapped movement exposed;
     - the old and new light-space bounds of casters that moved, appeared or vanished.
     
     Each dirty rectangle is cleared and culled per meshlet against its bounds, then
     drawn under a scissor.
   - **The light turning, a resize or a camera cut** redraws the cascade.
   - **Pros:** most of option 1's saving for the sun, with today's resolution, PCF and
     cascade selection. Its pieces are what option 1 needs anyway: stable light depth,
     dirty-region culling, tracking moved casters. Days of work.
   - **Cons:** no added resolution. Local lights are unchanged (their cache, ADR 0071, stays
     as it is).
3. **Staggered refresh:**
   - **What it is:** the far cascades drawn every 2nd, 4th or 8th frame (TESReloaded).
   - **Pros:** trivial; about half the cost.
   - **Cons:** moving casters in a stale cascade cast wrong shadows until its turn, and
     the cost stays proportional to the scene.
4. **As today**, with sample distribution fitting (ADR 0044's option 2) for sharper
   shadows. It saves nothing.

## Measurements / arguments

- **Today's cost** is in the research note (cascade counts, both GPUs, the defaults). The
  measurements were taken on ref-1 (RTX 4060 Ti, Radeon iGPU with driver 26.8.1),
  release, main at 60cb715.
- **Expected with option 2:**
  - **A still camera over a static scene** draws nothing in the shadow pass. On Intel's
    Sponza that is the RTX's 1.08 ms and the Radeon's 21.3 ms.
  - **A moving camera** draws strips whose width is its movement in texels:
    - at 5 m/s and 60 fps, about 8 cm a frame;
    - in a cascade 10 m across at 1024², 8 texels, 0.8 % of it;
    - the far cascades less.
  - **Moving casters** draw what their bounds cover twice (old and new).
  - These are estimates, to be measured on the `flight` example's path (a camera
    flying through Sponza) and on still shots.
- **Patents:**
  - **Warner Bros US 11,908,062 / 12,315,068** claim merging a static and a dynamic
    shadow map per texel through a dynamic mask, with deferred shadowing.
  - **Options 1 and 2 as written use one cache** whose regions are redrawn. There is no
    static/dynamic pair and no mask, and our shading is forward+.
  - **Prior art for caching and scrolling:** Insomniac 2012 and Unreal 4.9 (2015).
  - **The paging itself goes back to 2001–2007.** No paging patent turned up; a search
    by a lawyer is the owner's call.

## Recommendation (to confirm)

- **Option 2 now, then option 1's pages** where the cache leaves cost or resolution
  wanting. Pages would be added to the same cache: virtual resolution for the sun, then
  local lights.
- **First step, measured on both GPUs:**
  - stable light depth;
  - the array texture with wrap-around addressing;
  - scrolling with strips;
  - full redraws on the light turning or a cut.

  Then the moved casters' regions, through the GPU scene's changes and the draw list's
  previous transforms.
- **Golden images:** every frame must match an uncached render, so the golden shots add a
  moving camera and a moving caster. A test compares the cached and uncached results
  every frame.

## Questions for the owner

1. Scrolling cached cascades first (option 2), virtual pages later (option 1)? Or full
   virtual shadow maps now?
2. The patent stance as written: one cache, no static/dynamic pair merged by a mask.

## Decision

**Option 2 first** (the owner, 2026-10-14): scrolling cached cascades.
- Each cascade's depth is kept across frames in a layer of an array texture with wrap-around
  addressing.
- The light's depth range is fixed by the scene's bounds.
- Each frame draws only the strips the camera's movement exposed and the regions of
  casters that moved, culled per meshlet against them.
- The light turning, a resize or a camera cut redraws the cascade.

Virtual pages (option 1) come later, added to the same cache.

**One cache** (the owner, 2026-10-14): no static and dynamic pair merged per texel by a
mask (Warner Bros US 11,908,062 / 12,315,068).

## Consequences

- **With option 2:**
  - the shadow pass's cost follows what changes, not what is drawn;
  - golden tests need motion to cover it;
  - each cascade's memory stays as now (four 1024² layers);
  - the frame's cost varies more: the first frame and the light turning are the worst
    case, today's cost.
- **Option 1 later** adds a page table and pool, and replaces the cascades' fixed squares
  with clipmap levels.

## Step 1 as built (2026-10-14)

- **Where it lives.**
  - `engine_render::shadow_cache` plans the texels: which strips a cascade's window newly
    covers, split at the layer's seam into pieces.
  - `engine_render::forward::CascadeCache` is a view's map: a 2D array, a layer per
    cascade (16 MiB at 4 × 1024² of 32-bit depth).
  - A view passes it as `ForwardView::shadow_cache`. `engine_frame` gives every view one
    (`ViewSettings::cached_shadows`, on by default). A view without one, and a probe's
    capture, draws its cascades whole into a transient array, as before.
- **One map shape for both.** The 2 × 2 atlas is gone: every cascade is a layer, drawn in
  one pass with layered rendering. The vertex shader writes the layer, which needs
  `shaderOutputLayer` (now required, ADR 0006; both reference GPUs have it). The shading
  samples one `Texture2DArray` through a repeating comparison sampler. The descriptor stays
  the same for every pixel, so there is no non-uniform indexing.
- **Wrap-around addressing.**
  - Grid texel `g` of the light view lives at `(g − base) mod N` in its layer.
  - The base is reset whenever a layer is drawn whole, so a whole layer is always one
    piece.
  - A strip crossing the seam is drawn as up to 4 pieces. Each piece has its own viewport
    offset (`s0` or `s0 − N`) and a scissor, and is cleared before it is drawn.
  - The lookup's uv offset is `0.5 + s0 / N`.
- **Culling stays per cascade.** Each cascade has 2 light-view rectangles (the strips),
  and the instance cull and the meshlet cull test boxes and spheres against them.
  - The cascades' cull moved into its own entry points, `cullShadows` and
    `cullShadowsMeshlets`, so the view's cull lost its shadow branch. Radeon VGPRs: view
    cull 32 (unchanged), its meshlet path 40 → 35, the meshlet cull 56 → 55, the shadow
    vertex shader 45 → 43; the new shadow cull 43, with meshlets 46.
  - The shading's registers did not change on the Radeon (48 / 64). On the RTX they rose
    by one at most, within the same occupancy step.
- **What a cascade draws never depends on the camera.**
  - Its casters' detail levels follow its own texels: an error of at most
    `lod_threshold` texels. Before, they followed the camera's pixels. This applies to every
    cascade, cached or not.
  - Its depth range is the scene's.
  - Its reach is kept while it covers the view and is at most a quarter more than needed.
    A new reach gets a tenth more than needed. In a scene smaller than the shadow distance,
    the view's depth changes with every step.
  - Its radius has the same kind of hysteresis.
  - Together these keep kept texels valid. Without the reach hysteresis, walking through
    Intel's Sponza drew 24 cascades whole in 200 frames; with it, none.
- **Changes draw everything (this step).** The caster state covers:
  - for a draw list, a hash of meshes, their shapes' revisions, materials, transforms and
    the renderer settings that change what casts;
  - for a GPU scene, upload serials (the next upload with changes, or moving instances);
  - the material table's revision.

  Any change draws every cascade whole. The layers count as drawn once the pass records
  (its callback), so a frame never recorded leaves the cache as it was.
- **Tests.**
  - `cached_cascades_match_cascades_drawn_whole` covers 40 frames: walking, strafing,
    turning, standing still, a 316 m jump and a moving box. Each frame is compared with a
    cache that draws everything every frame, with the same fit.
    - RTX: 0 pixels differ in every frame. Radeon: at most 1 pixel in 3 frames, one of
      them a frame both draw whole (coplanar ties).
    - The cache drew 13.8 % of the texels. Still frames drew 0. The moving box drew
      everything.
  - `cached_cascades_follow_the_scenes_uploads` checks the scene path's serials and
    changes. What the cache holds shades identically to drawing everything.
  - The golden suite is unchanged on both GPUs.
- **Measured** (ref-1, release, 1920 × 1080, `scene_bench --sky --meshlet-culling
  --prepass [--cached-shadows] [--walk 0.05]`, 4 cascades of 1024², 200 frames; GPU
  shadow passes, median):

  | Scene, GPU | drawn whole every frame (still / walking) | cached, still | cached, walking 5 cm a frame | GPU frame whole → cached (still / walking) |
  |---|---:|---:|---:|---:|
  | Intel's Sponza, RTX | 884 / 802 µs | 0 | 65 µs | — |
  | Intel's Sponza, Radeon | 21.4 / 18.6 ms | 0 | 1.81 ms | 48.6 → 26.7 ms / 42.1 → 25.3 ms |
  | Sponza, RTX | 211 / 192 µs | 0 | 32 µs | 618 → 363 µs / 560 → 389 µs |
  | Sponza, Radeon | 2.49 / 2.37 ms | 0 | 0.55 ms | 16.1 → 13.5 ms / 15.7 → 13.8 ms |

  Walking, the cascades drew 0.6 % (Intel's Sponza) and 2.3 % (Sponza) of their texels
  per frame. Sponza's mean includes 3 reaches chosen anew.
- **The perf gate.** Its still shots keep drawing the cascades whole
  (`cached_shadows: false`), so the drawing stays measured against its baselines. Two
  new shots, `intel_sponza_walk_discrete` and `intel_sponza_walk_integrated`, walk the
  camera 5 cm a frame with the cache on.
- **Next (step 2).** Moved casters should draw their old and new light-space bounds only,
  not every cascade whole.
