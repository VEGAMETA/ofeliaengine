# Virtual shadow maps and cached shadows

Queue item 5 (`docs/status.md`). Researched 2026-10-14, after meshlets (ADR 0085): ADR 0044
kept virtual shadow maps for when the renderer could cull finely enough to fill pages.

## What our shadows cost today

The sun's 4 cascades of 1024² are redrawn from scratch every frame: every caster that
overlaps a cascade's square, in every cascade. Shadow pass with its culling, `scene_bench
--prepass --meshlet-culling --cascades N`, 1080p:

| scene | 1 cascade | 2 | 3 | 4 |
|---|---:|---:|---:|---:|
| Intel's Sponza, RTX 4060 Ti | 398 µs | 492 | 675 | 823 |
| Intel's Sponza, Radeon iGPU | 9.9 ms | 12.8 | 18.1 | 22.1 |
| Sponza, RTX | 40 µs | 72 | 108 | 133 |
| Sponza, Radeon | 0.69 ms | 1.31 | 2.15 | 2.50 |

With each device class's defaults (`perf_gate`, the GPU frame end to end):

- Intel's Sponza: the shadows are 50 % of the RTX's frame (1,079 of 2,152 µs) and 37 % of
  the Radeon's (21.3 of 57.9 ms).
- Sponza: the shadows are 11–15 % on the RTX and about 10 % on the Radeon.
- 10,000 detailed spheres: the shadows are 33 % of shadow + prepass + shading + culling on
  both GPUs.

None of this work changes from one frame to the next when nothing moves. The camera moves
the cascades by whole texels (ADR 0044's snapping), and nearly all of every cascade is the
same as the frame before.

## Sources

| Source | Date read | License | What it is |
|---|---|---|---|
| Epic Games, [Virtual Shadow Maps in Unreal Engine](https://dev.epicgames.com/documentation/en-us/unreal-engine/virtual-shadow-maps-in-unreal-engine) | 2026-10-14 | documentation (engine code not open source) | the production design: 16k² virtual maps, 128² pages, clipmaps, caching |
| Epic Games, [VSMs in Fortnite Chapter 4](https://www.unrealengine.com/en-US/tech-blog/virtual-shadow-maps-in-fortnite-battle-royale-chapter-4) | 2026-10-14 (403 for our fetcher; summarized from the documentation's references) | blog | the production tuning |
| K. T. Stephano, [Sparse Virtual Shadow Maps](https://ktstephano.github.io/rendering/stratusgfx/svsm) (StratusGFX) | 2026-10-14 | article; no license stated, ideas only | a full software-sparse implementation: page table entries, marking, allocation, invalidation, filtering at page borders |
| M. Sakmáry, J. Ryan, J. Hall, A. Lustri, "Virtual Shadow Maps", GPU Zen 3; M. Sakmáry, [master's thesis](https://dcgi.fel.cvut.cz/en/theses/2025/sakmamat/), CTU Prague 2025 | 2026-10-14 (abstract) | book chapter, thesis | page visibility from depth, fine-grained culling into pages, page caching, any number of cascades |
| [octoon/UnityVSM](https://github.com/octoon/UnityVSM) (after GPU Zen 3) | 2026-10-14 | "for educational purposes", no license: ideas only | 16 clipmap levels of 4096², 128² pages, a 2048-page pool, a hierarchical page buffer for culling, depth by atomic min |
| Blender, [EEVEE shadow module](https://developer.blender.org/docs/features/eevee/modules/shadow/) | 2026-10-14 | documentation of GPL code: **ideas only, no code carried over** | tile maps on the GPU without CPU sync; usage and update tagging; caching with throttled updates; up to 64 render views a frame |
| M. Acton (Insomniac), [CSM Scrolling](https://advances.realtimerendering.com/s2012/insomniac/Acton-CSM_Scrolling(Siggraph2012).pdf), SIGGRAPH 2012 Advances | 2026-10-14 (abstract) | talk | cached cascades scrolled with the camera, only the newly exposed edges drawn |
| Godot fork, [tile-based scrolling directional cache](https://github.com/NeoCry/godot_a/pull/62) | 2026-10-14 | MIT (Godot) | a cached far cascade scrolled by whole texels, only what needs it redrawn |
| Olsson, Billeter, Sintorn, Kämpe, Assarsson, [Efficient Virtual Shadow Maps for Many Lights](https://research.chalmers.se/en/publication/192178), I3D 2014; TVCG 2015 | 2026-10-14 (abstract) | paper | virtual cube maps for hundreds of local lights in bounded memory |
| Giegl, Wimmer, "Queried/Fitted Virtual Shadow Maps", I3D / GI 2007; Lefohn et al., "Resolution-Matched Shadow Maps", ToG 2007; Fernando et al., "Adaptive Shadow Maps", SIGGRAPH 2001 | — | papers | the technique's origins: pages where the view needs resolution |

## How they work

**Virtual shadow maps** (Unreal 5, StratusGFX, GPU Zen 3, EEVEE):

- **Virtual and physical memory.** Each light gets a large virtual map (Unreal 16k²; 4k² per
  clipmap level in UnityVSM) cut into pages of 128² texels. Only the pages that some
  pixel needs get physical memory, from one pool shared by every light.
- **The sun gets a clipmap.** Levels centered on the camera, each covering twice the area of
  the last, all at the same virtual resolution (Unreal levels 6–22: 64 cm to 40 km). A
  level is picked by distance or screen footprint: the texel density matches the pixels.
- **Marking.** Each frame a compute pass reprojects the depth buffer's pixels into the
  light and marks the pages they sample. EEVEE also tags for volumes and transparency.
  Unreal marks coarse pages everywhere for systems that sample anywhere (fog,
  translucency).
- **Allocation on the GPU.** Free lists or a page table updated with atomics (EEVEE: no
  CPU synchronization). StratusGFX started with a CPU readback and moved away from it.
  When the pool runs out, the oldest cached pages go first.
- **Caching.** A page stays valid across frames until something invalidates it:
  - the light moves or turns (every page of the light);
  - a caster moves, appears or disappears (the pages its bounds overlap);
  - a material moves vertices (Unreal: every frame).
  
  The clipmap's levels scroll with the camera by whole pages, so pages stay valid as it
  moves. Unreal optionally keeps separate static and dynamic depth copies (see patents).
- **Rendering pages.** Only dirty pages, and only what overlaps them. Unreal relies on
  Nanite's cluster culling: "VSMs are designed for Nanite… non-Nanite geometry is much
  more expensive". UnityVSM culls against a hierarchical page buffer. Depth is written
  with atomic min into the physical pool (no early Z) or through per-page viewports.
- **Sampling.** Shading translates virtual to physical through the page table (one
  indirection). Hardware PCF breaks at page borders: either borders around pages, software
  filtering near them, or software filtering everywhere. Unreal filters by ray marching
  the map (SMRT), soft and contact-hardening, 8 rays by default.

**Scrolling cached cascades** (Insomniac 2012, the Godot fork, others' staggered refresh):

- Each cascade keeps its depth from the frame before. When the camera moves the cascade by
  whole texels, the map scrolls and only the newly exposed strips are drawn.
- Moved casters redraw the area they covered and the area they cover now.
- The light turning, or the cascade's size changing, redraws the cascade.
- The same resolution as cascades: no page table, no marking, no pool.
- Some engines simply refresh the far cascades every 4 or 8 frames (TESReloaded). That is
  cheap but wrong for moving casters until the next refresh.

## Patents

- **Warner Bros, US 11,908,062 B2 and US 12,315,068 B2** ("Efficient real-time shadow
  rendering", priority 2019-07-26, active, about 2041). Claims 1 and 11: filtering each
  texel of a frame's shadow map either from a static plus a dynamic shadow map or from the
  static one alone, chosen by a **per-texel dynamic mask**, then rendering with
  **deferred** shadowing. Claim 12: shadow maps of different resolutions per light,
  compressed by quantizing their texels, arranged for runtime.
  - **We do not build a static and a dynamic map merged per texel through a mask** (Unreal's
    "separate static caching" is that shape). A single cache whose pages or regions are
    redrawn when casters move is a different method.
  - Our shading is forward+, not deferred shadowing.
  - We do not quantize shadow texels.
- **Prior art for caching and scrolling:** Insomniac's CSM scrolling (2012) and Unreal 4.9's
  cached static shadows (2015) predate that priority. The paged technique itself goes back
  to adaptive (2001), resolution-matched and virtual shadow maps (2007).
- **No patent claiming virtual shadow map paging** turned up (Google Patents and USPTO
  searches, 2026-10-14). A full search is a lawyer's job; the owner decides as with ADR
  0035.
- **Microsoft US 10,672,182** (ADR 0035) is about persistent per-triangle visibility. A page
  table that persists across frames is shadow-map residency, not triangle visibility. The
  culling into pages keeps nothing per meshlet from frame to frame, as ADR 0085 does.

## What we take

- **Caching across frames, invalidated by caster bounds and by the light.** This is where
  the cost goes away: a still scene costs only its sampling.
- **Fine culling into what is redrawn,** through the meshlet cull we have: a dirty region's
  bounds in light space are what each meshlet sphere is tested against.
- **Scrolling by whole texels or pages** with the camera, as both families do.
- **Later, pages:** the virtual resolution's sharper shadows near the camera, and local
  lights' maps sized by their screen footprint (Olsson et al.).

## What we don't take

- **A static and a dynamic map combined per texel by a mask** (the patent).
- **Hardware sparse residency** (StratusGFX measured it slow on Windows; AMD's iGPU support
  is unknown): software paging over a pool texture, if pages come.
- **SMRT filtering:** our 3 × 3 PCF stays until soft shadows are their own item.
