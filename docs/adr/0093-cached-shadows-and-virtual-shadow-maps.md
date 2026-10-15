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
    (`ViewSettings::cached_shadows`, on by default).
  - A view without one, and a probe's capture, draws its cascades whole every frame into
    the transient atlas, as before: tiles of one layer, one rendering.
- **The shading reads both alike.** The map is a `Texture2DArray`; each cascade has its
  layer and its uv transform (its tile, or where its window starts in its layer). The
  comparison sampler repeats. The descriptor is the same for every pixel, so there is no
  non-uniform indexing. The shading's registers did not change (Radeon 48 / 64).
- **A cache's layers are drawn one at a time, each in a rendering of its own.** The
  frame graph's new `PassBuilder::depth_layers` declares the pass, and the pass begins
  and ends each layer itself through the graph's per-layer views. Two ways measured
  worse on the RTX 4060 Ti, Sponza, 4 cascades:
  - **one layered rendering** (the vertex shader picking the layer): shadow pass
    +18 % (158 → 186 µs), likely its depth culling across layers;
  - **a rendering per layer for the atlas too:** with meshlets, about +4 µs a cascade
    (1 / 2 / 4 cascades: +0 / +5 / +13 µs).

  So views without a cache keep the atlas, and caches pay the renderings: a strip frame
  draws little, and step 2 removes most whole redraws.
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
    `cullShadowsMeshlets`, so the view's cull lost its shadow branch (Radeon VGPRs of its
    meshlet path 40 → 35).
  - **The meshlet cull's registers move other passes.** A version with fewer registers on
    the RTX (47 → 40) ran more groups at once. That shuffled the draws they append, which
    cost the depth tests of the passes drawing them: Sponza with each class's defaults,
    prepass +8–9 % and shadows +9–13 %. The rectangles are now read where main read the
    bounds (48 registers), and both passes are back to main's. Order-preserving appends
    would remove this fragility (status §6).
- **What a cascade draws never depends on the camera.**
  - Its casters' detail levels follow its own texels: an error of at most
    `lod_threshold` texels. Before, they followed the camera's pixels. This applies to every
    cascade, cached or not.
  - Its depth range is the scene's.
  - Its reach is kept while it covers the view and is at most a quarter more than needed.
    The first reach is what the view needs, as without a cache. One the view outgrows is
    chosen again with a tenth more. In a scene smaller than the shadow distance, the view's
    depth changes with every step.
  - Its radius has the same kind of hysteresis.
  - Together these keep kept texels valid. Without the reach hysteresis, walking through
    Intel's Sponza drew 24 cascades whole in 200 frames; with it, the 4 of the first
    frame and the 4 of the first step that outgrew it.
  - A camera cut (in `engine_frame`) and any change that draws everything also forget the
    hysteresis: the fit is then a new view's.
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
    - RTX: 0 pixels differ. Radeon: at most 1 pixel in 3 frames, one of them a frame
      both draw whole (coplanar ties).
    - The cache drew 13.8 % of the texels. Still frames drew 0. The moving box drew
      everything.
  - `cached_cascades_follow_the_scenes_uploads` checks the scene path's serials and
    changes. What the cache holds shades identically to drawing everything.
  - The golden suite and `engine_frame`'s cut test pass on both GPUs.
- **Measured** (ref-1, release, 1920 × 1080, `scene_bench --sky --meshlet-culling
  --prepass [--cached-shadows] [--walk 0.05]`, 4 cascades of 1024², 200 frames). GPU shadow
  passes (with their culling), median, and the GPU frame:

  | Scene, GPU | drawn whole (still / walking) | cached, still | cached, walking 5 cm a frame | GPU frame whole → cached (still / walking) |
  |---|---:|---:|---:|---:|
  | Intel's Sponza, RTX | 824 / 728 µs | 0 | 69 µs | 1,520 → 692 µs / 1,312 → 650 µs |
  | Intel's Sponza, Radeon | 21.7 / 18.9 ms | 0 | 1.98 ms | 50.6 → 28.5 ms / 43.7 → 27.0 ms |
  | Sponza, RTX | 135 / 129 µs | 0 | 32 µs | 516 → 366 µs / 499 → 389 µs |
  | Sponza, Radeon | 2.49 / 2.46 ms | 0 | 0.56 ms | 16.9 → 14.8 ms / 16.6 → 14.9 ms |

  - Walking, the cascades drew 1.15 % (Intel's Sponza) and 1.30 % (Sponza) of their
    texels per frame.
  - Views without a cache, against main (A/B, RTX, Sponza): 161 → 162 µs plain, 134 →
    135 µs with meshlets and the prepass.
- **The perf gate.**
  - Its still shots keep drawing the cascades whole (`cached_shadows: false`), so the
    drawing stays measured against its baselines.
  - Two new shots, `intel_sponza_walk_discrete` and `intel_sponza_walk_integrated`, walk
    the camera 5 cm a frame with the cache on.
- **Next (step 2).** Moved casters should draw their old and new light-space bounds only,
  not every cascade whole.

## Step 2 as built (2026-10-15)

- **What moved draws its places, not every cascade.**
  - The changes come from the draw list (`DrawDiff`, shared with the local lights'
    cache: a draw whose mesh, material, transform or mesh shape changed, or that appeared
    or went) or from the GPU scene (an upload's changes and the instances moving between
    ticks).
  - Their old and new world boxes go into each cascade's light view (Arvo), become
    window rectangles a texel larger, and are gathered into clusters after the strips: at
    most 4 rectangles a cascade (`Cascade::drawn`).
  - Still drawn whole: another draw list or scene, renderer settings that change what
    casts, edited materials or meshes, and more than 512 changes waiting.
- **Each piece used to draw every caster of its cascade.** Culling is per cascade,
  against all its rectangles, so each piece (a rectangle's part on one side of the
  layer's seams) drew all of them under its own scissor. Per cascade, Radeon, Sponza,
  meshlets on:
  - walking, the nearest cascade cost 55 % of its whole redraw;
  - with 8 moving casters and walking, the farthest cost 163 %.
- **A cascade's pieces are drawn at once** with the meshlet path, where vertex shaders
  pick viewports (`OptionalCaps::viewport_index`: `multiViewport`,
  `shaderOutputViewportIndex`, 16 viewports; both reference GPUs):
  - `cullMeshletPieces` tags each run of meshlets with the pieces it reaches. The tag is
    16 bits: bit `4r + q` is rectangle `r`'s part in quadrant `q` of the layer, cut at the
    seams' light-view lines. It goes beside the run's slot, and the run's instance count
    is the number of pieces. Runs split where the pieces change.
  - `shadowVertexPieces` and `shadowVertexMaskedPieces` read the pieces at the draw's
    first instance and send instance `i` to the `i`-th piece's viewport.
  - The pass sets 16 viewports and scissors, clears every piece at once and draws the
    cascade's meshlet lists once.
  - Instanced draws (meshes without meshlets, material types) stay piece by piece, as
    does everything on devices without the capability.
  - Their own entry points, and pipelines made on a cache's first frame: none of it runs
    in a view without a cache, and the meshlet cull and the shadow vertex shaders keep
    their registers.
  - Radeon, Sponza, walking, per cascade: 241 → 96, 258 → 89, 140 → 56 and 29 → 24 µs.
- **The guard.** A plan's cost against a whole redraw is the casters its rectangles
  reach: about 3 times their share of the window (calibrated on Intel's Sponza), drawn
  once with the pieces at once, else once per piece. Above 0.8 the cascade is drawn
  whole. With the pieces at once, the clusters may take every rectangle the strips leave;
  otherwise two.
- **The cascade grew** to 144 bytes (four rectangles, their count, the seams, the
  pieces):
  - the culls test the first two rectangles as in step 1 and the clusters after the
    strips only when a cascade has more (`Cascade::rects`). Testing all four took the
    Radeon's meshlet cull from 53 to 64 VGPRs (its pass +14 %, +75 µs on Intel's Sponza)
    and the RTX's instance cull from 40 to 48 registers; both are back to step 1's, the
    pass +2 % (11 µs on Intel's Sponza, Radeon);
  - the shaders read a cascade's fields one at a time (loading a whole cascade cost the
    Radeon's shading 6 registers), and its layer comes from the frame
    (`cascade_layers`);
  - the shading looks the sun's shadow up before the lobes' terms; with decals (a
    literal), after them, where the decals' values are live. Step 1's lookup had taken
    the Radeon's environment variant from 64 to 70 VGPRs; it is at 64 again: Intel's
    Sponza without a cache, forward 22.7 → 21.6 ms on the Radeon, 461 → 455 µs on the
    RTX. On the RTX the variants with motion vectors (TAA without the depth prepass)
    trade: with the environment 56 → 64 registers (Sponza, forward 394 → 401 µs), with
    local lights 63–64 → 56. Looking the shadow up later for them would trade back;
  - `shadow_distance` reads the map as the array it is, wrapping as the lookups do.
- **Tests.**
  - `cached_cascades_match_cascades_drawn_whole` also moves a box while walking, and runs
    twice: instanced, and by meshlet with spheres for the poles (125 cascades drawn in
    pieces at once). At most 1 pixel differs on either GPU; the cache draws 5.8 % of the
    texels.
  - `cached_cascades_follow_the_scenes_uploads`: an upload that moves an instance draws
    a part of the cascades.
  - `shadow_cache`'s unit tests: the moved casters' places over a 300-step random walk
    (both ways of drawing), the clusters, the guard.
- **Measured** (ref-1, release, 1920 × 1080, `scene_bench --sky --meshlet-culling
  --prepass --cached-shadows [--walk 0.05] [--movers n]`, 4 cascades of 1024², 200
  frames). GPU shadow passes, median; the movers are the `n` smallest draws, each swinging
  25 cm either way along x over 120 frames:

  | Scene, GPU | drawn whole (still / walking) | walking: step 1 → 2 | 1 mover (still / walking) | 8 movers (still / walking) |
  |---|---:|---:|---:|---:|
  | Intel's Sponza, RTX | 827 / 728 µs | 69 → 43 µs | 17 / 50 µs | 317 / 403 µs |
  | Intel's Sponza, Radeon | 21.8 / 19.0 ms | 1.98 → 0.76 ms | 0.21 / 0.88 ms | 5.55 / 7.03 ms |
  | Sponza, RTX | 147 / 130 µs | 32–35 → 24 µs | 15 / 25 µs | 54 / 53 µs |
  | Sponza, Radeon | 2.51 / 2.43 ms | 0.56 → 0.24 ms | 0.07 / 0.26 ms | 0.64 / 0.90 ms |

  - With step 1, every frame with a moving caster drew every cascade whole, through the
    layers: Radeon, Sponza, 2.70 ms against the atlas's 2.54 (a rendering per layer,
    +6 %); the RTX the same.
  - Views without a cache, step 1 → step 2 (A/B, alternated): Sponza RTX 162 → 162 µs,
    with meshlets 135–147 both; Radeon 2.82 → 2.82 ms, with meshlets 2.50 → 2.50 ms.
  - The perf gate passes on both GPUs. Its walking shots' shadow pass: RTX 80 → 39 µs,
    Radeon 1.46 → 0.57 ms. Against step 1 (`perf_gate --scene`, 2 alternated rounds),
    GPU frame: RTX intel_sponza_defaults_discrete 2,147 → 2,139 µs,
    sponza_defaults_discrete 1,341 → 1,336 µs, sponza_taa 860 → 869 µs (its environment
    variant with motion vectors, above); Radeon intel_sponza_defaults_integrated 57.42 →
    57.46 ms, sponza_defaults_integrated 22.07 → 22.03 ms.
- **Left.** Instanced draws still draw each piece's casters again (the same tags could
  cull them per piece). The clusters are greedy. A rendering per layer costs the Radeon
  6 % when everything is drawn whole (the first frame, a cut, the light turning).

## Step 3: virtual pages for the sun (design, 2026-10-15)

The owner put the pages after the player crate (2026-10-15). The cache's cost is low now
(a still camera draws nothing, a walk 0.8–2 % of the texels); what it leaves wanting is
**resolution**. With the defaults a shadow texel is never smaller than 2.2 pixels, and is
4–10 pixels at 2–5 m from the camera (the research note's table). Pages give each pixel
about a texel and back only what pixels sample.

**Options for the parts:**

1. **Drawing into pages.**
   - (a) **A rendering without attachments over a level's virtual 4096², the fragment
     shader writing the depth with an atomic minimum into the pool** through the page
     table, only into dirty pages (Timberdoodle, StratusGFX, UnityVSM). No limit on pages
     a frame; no early depth test (depth only: cheap fragments).
   - (b) A viewport per dirty page and the pool as the depth attachment: early depth
     tests, but at most 16 viewports a draw (`multiViewport`), so the draws repeat per
     group of pages.
   - (c) Mesh shaders culling triangles per page: not on the Radeon today (#88).
   - **Chosen: (a)**, with our meshlet cull (ADR 0085) choosing what each level draws.
     It needs `fragmentStoresAndAtomics` (every desktop GPU), enabled when present.
2. **Allocation.** (a) On the GPU, from a scan of the physical pages (Timberdoodle);
   (b) through a readback to the CPU (StratusGFX's first version: a frame late).
   **Chosen: (a).**
3. **Shading.**
   - (a) Every shading variant looks the pages up itself: a twin of every variant, and
     registers in the forward shader (ADR 0058's rule).
   - (b) **A screen-space pass after the pages are drawn** writes the sun's visibility,
     filtered, into the mask the contact shadows write today (ADR 0071). The variants
     that read that mask skip the cascades' lookup: fewer registers than now.
     Transparent surfaces have no depth in the prepass and keep a lookup of their own.
   - **Chosen: (b).** One cache and a visibility mask is not the static and dynamic pair
     merged per texel that Warner Bros' patents claim.
4. **Filtering.** The 3 × 3 tent PCF the cascades have, each tap through the page table
   when the footprint crosses a page's edge (Timberdoodle's way). A 1-texel border per
   page is the alternative if the lookups cost.

**The design:**

- **Levels.** 16 levels of 4096² virtual texels in 64² pages (a 64 × 64 page table
  each). Level `k` covers `2 m × 2^k` around the camera. The first texel is 0.5 mm; the
  last level is 65 km across. Each level's window snaps to whole pages and its page table
  is addressed modulo 64 (scrolling frees the pages that wrap). The light's depth range is
  the scene's, as the cascades' is.
- **The pool.** 4,096 pages of 64² in one `R32_UINT` image of 4096² (64 MiB), and one
  meta entry per page (the virtual page it backs, the last frame a pixel needed it).
  Per device class once measured.
- **Each frame**, after the depth prepass:
  1. levels scroll; wrapped pages are freed;
  2. pages are invalidated: all of them when the light turns or its depth range changes,
     those under a moved caster's old and new light-space boxes otherwise (the GPU scene's
     changes, as the cascades take them);
  3. marking: each pixel's level (from its distance, the finest that gives about a texel
     per pixel), its page flagged as needed, a first request appended;
  4. allocation: free pages first, then those no pixel needed for a few frames;
  5. new and dirty pages cleared to the far plane;
  6. per level, a hierarchy of dirty bits; the meshlet cull keeps what touches dirty
     pages; the levels with dirty pages are drawn (an indirect draw list each);
  7. the visibility pass fills the sun's mask; the shading reads it.
- **Fallback.** A pixel whose page could not be allocated uses the next coarser level that
  has one (and the coarsest levels can be kept resident).

**Steps:**

- **3a.** The pages' machinery without drawing: pool, tables, scrolling, marking,
  allocation, invalidation, clearing. Tested against a CPU model of the same rules: a
  depth buffer marks the pages it should; allocation fills them; still frames allocate
  nothing; scrolling frees what wraps; the pool's overflow fails gracefully.
- **3b.** Drawing and shading: culling into dirty pages, the attachment-less drawing, the
  visibility pass, the shading variants reading the mask. Golden shots near the camera
  and in the distance, a test against a high-resolution reference map, perf gate scenes,
  register checks on both GPUs.
- **3c.** Moved casters and the light turning, defaults per device class, measurements
  against the cascade cache, documentation.

## Step 3a as built (2026-10-15)

`engine_render::vsm::VirtualShadows` and `vsm.slang`. Nothing draws into the pages or reads
them yet: 3b wires them into the frame.

- **One graph pass of seven dispatches**, after the depth prepass:
  1. the frame's flags cleared, the entries whose window wrapped freed;
  2. marking;
  3. the free pages and the old ones (no pixel needed them for `keep_frames` frames);
     when the light turned, the needed pages dirty and the others freed;
  4. allocation: free pages, then old ones, then failure;
  5. the dirty pages collected;
  6. the clear's dispatch;
  7. the clear: one group per dirty page, indirect.
- **Kept across frames:** the pool (`R32_UINT`, rows of 64 pages, in the general layout),
  the page tables (a 64 × 64 table per level), and a meta entry per physical page (the
  entry it backs, the last frame a pixel needed it).
- **Changes from the design:**
  - **The light turning frees the pages no pixel needs**, rather than marking every page
    dirty: a dirty page is drawn in its frame, and a page nobody samples would be drawn
    for nothing. In the test's turn at 540p, 852 pages are drawn instead of 1,886.
  - **A page is dirty in one frame:** the frame that draws it. A request that finds no
    page fails, and is asked for again the next frame.
  - **The marking's arithmetic.** Clip → light and clip → camera-relative matrices are
    made on the CPU. A pixel takes one reciprocal; the level comes from the squared
    distance, and the page from pages per metre (no divisions).
  - **The pool takes any number of pages,** up to 65,472, as many rows as the device's
    images allow.
- **Tests** (`tests/render/vsm.rs`, both GPUs, synchronization validation).
  - **Each frame's change from the last is held to the rules:**
    - the pages marked, against the same math in f64 (a pixel within 1 % of a page's or a
      level's edge may land on either side);
    - the requests, the free and old lists, allocation, failures and eviction;
    - kept entries and their meta;
    - the dirty pages, cleared to the far plane.
  - **Scenarios:**
    - still frames allocate and clear nothing;
    - walking and a 1 km jump free what wraps;
    - the light turning;
    - a pool with room for one of two views fails for `keep_frames` frames after turning
      around, then takes the old pages.
- **Numbers** (`what_the_pages_cost_at_1080p`): a ground plane seen from 1.7 m at
  1920 × 1080, default settings (16 levels, 4,096 pages, 64.6 MiB).

| | RTX 4060 Ti | Radeon iGPU |
|---|---:|---:|
| first frame (3,185 pages backed and cleared) | 121 µs | 1,879 µs |
| still camera | 62 µs | 1,029 µs |
| walking at 3 m/s | 64 µs | 1,047 µs |
| of which marking | 51 µs | 985 µs |

- **The clear's first version** had each lane clear 8 × 8 texels, lanes 8 texels apart:
  the first frame took 892 µs on the RTX and 11.67 ms on the Radeon. Clearing one 8 × 8
  block a step brought them to 116–121 µs and 1.88 ms.
- **The Radeon's marking is bound by arithmetic:** about 120 lane-cycles a pixel on its
  2 CUs, about its instruction count.
  - Not the depth reads: without them it is 4 % faster.
  - The per-wave deduplication and its atomics are about 20 % of it.
  - The matrices made on the CPU took 10 % off.
  - Not tried: 2 × 2 pixels a thread, to share the deduplication.
- **For 3c.** The ground plane fills 78 % of the default pool at 1080p; grazing views back
  the most pages. The pool's size and the level bias (+1 quarters the pages) are per
  device class. The integrated class keeps the cascades unless 3c measures otherwise.

## Step 3b as built (2026-10-15)

The pages are drawn and read: `ViewSettings::virtual_shadows` (off by default) gives a
view's opaque surfaces the sun's shadow from them. Transparent surfaces and subsurface
transmission keep the cascades, which stay drawn (cached) beside the pages.

- **Marking with the filter's footprint.** A pixel takes the first level whose window
  holds its whole footprint, its texel ± 4 texels, and marks the up to 2 × 2 pages that
  footprint touches (one loop per wave over its distinct pages). It cost the Radeon
  0.22 ms more on 3a's ground plane at 1080p (1.35 ms a still frame); the RTX 65 µs.
- **A hierarchy of dirty pages per level** (64² down to 1², 5,461 cells, indexed by the
  page's place in the window), built where the dirty pages are collected.
- **The depth mapping.** Depth along the light over the scene's range rounded out to
  16 m steps, and a step more: the range moves rarely, and a change of it counts as the
  light turning.
- **Casters into the dirty pages** (`forward.slang`):
  - `cullVsm`, a thread per instance: its box against each level's window (the
    light's rows) and the hierarchy, at most 4 levels an instance; its meshlets go to
    `cullVsmMeshlets`, or it is drawn whole.
  - `cullVsmMeshlets`: each meshlet's sphere against the dirty pages and its cone;
    runs of meshlets become one draw.
  - The draws go to a list of their own (`VSM_LIST`, 2^19 draws at most; each draw's
    word is its batch and level), with its own batch table.
  - `vsmVertex` / `vsmFragment` (and their alpha-tested twins): one rendering of 4096²
    without attachments, the fragment's depth written with an atomic minimum into the
    pool, only in pages that are backed and dirty. The light's y is flipped so the
    winding matches the cascades: the faces turned away from the light are drawn.
- **The visibility** (`vsmVisibility`, after the contact shadows): per pixel of the
  prepass's depth, the level it marked; the normal from the depth (on each axis the
  neighbour nearer in depth); the receiver moved along it by a texel of its level, and
  its depth biased by half a texel; a 3 × 3 tent filter of comparisons over 4 × 4
  texels, as the cascades weigh their 3 × 3 bilinear comparisons, each texel read through
  the page table (at most 4 entries). Written into the sun's mask, or multiplied into what
  the contact shadows wrote there.
- **The shading** uses the variants that read the mask. A bit of the shading's `phase`
  (`SUN_IN_MASK`) says the mask holds the sun's whole visibility: they look up no cascade.
  Their registers are unchanged on both GPUs (`scene_bench --pipeline-stats` against main);
  no pipeline of the perf gate's scenes uses more.
- **Which views.** Single-sampled perspective views with a depth prepass (it then runs);
  multisampled views keep the cascades. The device needs `fragmentStoresAndAtomics`,
  `drawIndirectCount` and `shaderInt64`; without them the view keeps the cascades.
- **Casters that change redraw every page** a frame needs, and free the others, as the
  light turning does: a draw list's changed draws, a scene upload's changes, instances
  moving between ticks, another list or scene, other materials or caster settings
  (`VirtualShadows::invalidate` for what the renderer cannot see). 3c redraws only the
  pages under what changed.
- **Tests** (both GPUs, synchronization validation):
  - `tests/render/vsm.rs`: 3a's model, with the margin's pages and the hierarchy.
  - `tests/render/vsm_draw.rs`: a ground slab, a plate in the air and a box; the camera
    stands, stands still and steps aside, then the plate moves. Every texel of every
    backed page holds the depth where the light's ray through it leaves the first box it
    meets (none differs); each pixel's visibility matches a ray from its surface to the
    sun outside the filter's penumbra (none differs). Without the redraw on change, the
    frame after the move has 12,314 wrong texels.
  - Golden shots on both GPUs (`*_vsm`): the procedural scene near (with and without
    contact shadows), Sponza far, Sponza by day with everything.
- **The picture.** Shadow edges are sharper, thin shadows the cascades lose appear (a
  cornice's on the wall below it), and the cascades' dithered band at their edges is
  gone. Against the cascades, by day: Intel's Sponza darker on 1.6 % of the pixels and
  lighter on 0.1 %; Crytek's darker on 0.3 % and lighter on 1.0 %. The pages hold what
  rays find (the test above); the cascades' coarser texels and wider normal offset miss
  both ways.
- **Numbers** (the perf gate's scenes, 1920 × 1080; Intel's Sponza with each class's
  defaults; medians of 3 runs):

| µs a frame | RTX 4060 Ti | Radeon iGPU |
|---|---:|---:|
| still: marking and bookkeeping (`vsm pages`) | 72 | 1,861 |
| still: culls and draws (nothing dirty) | 10 | 13 |
| still: visibility | 164 | 13,469 |
| the forward pass without the cascades' lookup | −34 | −1,356 |
| walking 5 cm a frame: drawing (`vsm draw`) | 885 | 27,669 |
| walking: the cached cascades' strips instead | 68 | 752 |
| walking, integrated class (67 %): drawing / visibility | 608 / 73 | 18,712 / 6,701 |
| GPU frame still: pages / cascades drawn whole every frame | 1,610 / 1,440 | 66,904 / 58,095 |
| GPU frame walking: pages / cached cascades | 2,490 / 1,372 | 91,054 / 53,973 |

- **What 3b leaves** (for 3c, in order of their cost):
  1. **Drawing while walking** costs 13 times the cached cascades' strips. The level's
     whole 4096² is rasterized for every drawn triangle and the fragments outside dirty
     pages discarded: a meshlet that reaches one dirty page rasterizes all its
     triangles, and in the fine levels a floor's triangle covers millions of texels.
     Likely fix: clip each draw to the dirty pages it reaches (a rectangle per draw,
     clip distances), so only dirty texels are rasterized.
  2. **The visibility on the Radeon** (13.5 ms at 1080p; the RTX 164 µs): 16 taps through
     the page table and five reconstructed positions a pixel. Candidates: hardware
     comparisons on a float view of the pool with a texel of border per page, the
     normal from fewer reads.
  3. **Moved casters** redraw every needed page; 3c redraws the pages under their old
     and new places.
  4. **Transparent surfaces and subsurface transmission** read the cascades, so they are
     still drawn.
  5. **Defaults per device class.** Off everywhere until the pages cost less than the
     cascades they replace on that class.

## Step 3c, first part (2026-10-15): drawing and the visibility made cheaper

**Drawing (3c-1).** While walking, the pages' draw cost 13 times the cached cascades'
strips. Taking parts out showed where the time went (Intel's Sponza walking, RTX):

| vsm draw, µs | RTX |
|---|---:|
| as built in 3b | 885 |
| fragment shaders empty | 880 |
| every triangle clipped away in the vertex shader | 8 |

Rasterization was the cost: a drawn triangle covered its level's whole 4096², and the
fragments outside dirty pages were discarded after the fact. Now each draw is clipped to
the dirty pages it may change.

- **The level's dirty pages also as bits**, a row of the window in two words, after the
  hierarchy's mips; `vsmCollectDirty` sets them.
- **The culls give each draw the box of the dirty pages under it**, a read a row
  (`vsm_dirty_rect`), and a run of meshlets the union of its meshlets'. It goes in the
  draw's second visible slot.
- **The vertex shaders clip to it** with four clip distances. `shaderClipDistance` is
  now an optional cap the pages need.
- **Coarser boxes, from the hierarchy's cells**, were measured first: 164 µs (cells a
  quarter of the caster), 111 (an eighth), 84 (a sixteenth). Exact boxes from the rows
  give 68 µs. One draw per meshlet instead of runs: 66 (noise).
- **Two Radeon-only failures on the way**, both changed back:
  - the row words read through a `uint2` pointer read wrong data (now two scalar loads);
  - the masked fragment returning before its alpha test (to skip clean pages' texture
    reads) lost the Radeon's device in Sponza; the alpha test stays first.

**The visibility (3c-2).** On the Radeon the pass cost 13.5 ms at 1080p (the RTX 163 µs).

- **What it cost**, taking parts out:
  - the normal, from four neighbouring depths read and transformed per pixel: 6.2 ms, and
    102 VGPRs live;
  - the level search with the page table: about 3.7 ms;
  - the filter's 16 taps: the rest.
- **The changes:**
  - a group of 8 × 8 pixels reads its tile and a pixel around it (100 depths) into
    group-shared memory once, with their positions: 33 VGPRs;
  - the levels' windows and scales are in group-shared memory for the level search;
  - a footprint in one page (most pixels) reads its table entry once;
  - the 16 pool reads go out without a branch.
- **Tried and left out:**
  - two pixels a thread (Radeon −1 %, RTX +7 %);
  - bindless indices without `NonUniformResourceIndex` changed nothing (they are kept
    without it, as the other passes index uniform push constants).
- **What it is bound by now:** the latency of its chain of reads (depth, page table,
  pool). Without the pool's reads it takes 3.3 ms, with one of them 5.5 ms, with 16 of
  them 7.0 ms.
- **Not tried yet:**
  - the table entries read before the group's barrier (from the unoffset position, a
    texel wider);
  - gathers on a sampled view of the pool;
  - the visibility at a lower resolution.
- **The golden images move by rounding** (mean FLIP ≤ 0.0005: how the compiler fuses the
  positions' arithmetic) and were written again. The test against rays: no pixel
  differs.

**Numbers** (the perf gate's scenes, 1920 × 1080, Intel's Sponza; 3b → now):

| µs a frame | RTX 4060 Ti | Radeon iGPU |
|---|---:|---:|
| walking: vsm draw | 885 → 69 | 27,669 → 2,606 |
| walking: vsm cull | 13 → 17 | 75 → 123 |
| still: vsm visibility | 164 → 152 | 13,469 → 6,948 |
| integrated class (67 %) walking: draw / visibility | 608 / 73 → 34 / 68 | 18,712 / 6,701 → 943 / 3,650 |
| GPU frame walking: pages / cached cascades | 2,490 → 1,650 / 1,372 | 91,054 → 64,060 / 53,973 |
| integrated class walking: pages / cached cascades | 1,436 → 862 / 705 | 52,983 → 32,544 / 29,641 |

Walking, the pages now cost the RTX 278 µs more than the cached cascades (their
visibility 149, bookkeeping 73, draw 69 and cull 17, less 32 the shading saves), and the
Radeon 2.9 ms more with the integrated class's settings.

## Step 3c, second part (2026-10-15): moved casters redraw only their pages

Until now any caster that changed redrew every page a frame needed. The pages now keep, as
the cascades' cache does, the world boxes of what moved, appeared or went since they were
drawn, at their old and new places:

- a draw list's changed draws;
- a scene upload's changes;
- instances moving between ticks.

The frame uploads them as rectangles across the light. `vsmFindPages` then treats a backed
page under one as it treats every page when the light turns: drawn again if a pixel needs
it, freed if not. Every page a frame needs is still drawn again for another list or scene,
other materials, or more than `MAX_MOVED` (256) boxes.

- **The test:** after the plate moves, `vsm_draw.rs` draws 12 of 125 backed pages (before:
  every needed page, about 100), and every texel still matches the rays on both GPUs.
- **Measured** with `scene_bench --vsm` (new: the pages with `--movers` and `--walk`, their
  passes in the shadow column). Intel's Sponza, `--sky --meshlet-culling --prepass
  --cached-shadows`; GPU shadow passes, the cached cascades included (transparent surfaces
  read them):

| µs a frame | RTX 4060 Ti | Radeon iGPU |
|---|---:|---:|
| still: cascades / with the pages | 0 / 225 | 0 / 8,592 |
| 1 mover: cascades / pages, all redrawn / pages, theirs only | 17 / 828 / 257 | 203 / 28,304 / 9,773 |
| 8 movers: cascades / pages, all redrawn / pages, theirs only | 317 / 1,128 / 603 | 5,625 / 33,634 / 17,206 |

**Left for later:**

- **Transparent surfaces and subsurface transmission still read the cascades**, so a view
  with pages draws both. With 8 movers that is 317 of the RTX's 603 µs. Reading the pages
  in the forward shading needs variants of its own (the registers rule) and pages for
  surfaces the prepass does not see.
- **The sun turning redraws every page the frame needs**, as the cascades' cache does: a
  time of day should turn it in steps.
- **Defaults per class:** the pages stay off by default. With the integrated class's
  settings they cost the Radeon 2.9 ms more than the cached cascades while walking.
  Turning them on by default for discrete GPUs trades about 0.25–0.3 ms of an RTX 4060
  Ti's frame for the sharper shadows: that is the owner's decision.
