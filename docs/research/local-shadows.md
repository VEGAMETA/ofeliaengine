# Shadows of point and spot lights, and contact shadows

Checked 2026-10-01. Sources at the end. Bend Studio's screen-space shadows were adapted
(Apache-2.0, see THIRD_PARTY_NOTICES.md); nothing else was copied: the other techniques come
from papers, talks and documentation.

## Why

Only the sun cast shadows (cascaded shadow maps, ADR 0044). Point and spot lights (ADR
0045) lit through walls, pillars and characters. A game's lamps, torches, flashlights and
muzzle flashes need shadows, but a shadow is a depth render of the scene per light (six for
a point light): drawing all of them every frame is out of reach on the Radeon iGPU (Sponza:
~70 µs per view there, ~2.5 µs on the RTX 4060 Ti). Most lights and most of the scene do not
move, so a shadow drawn once stays right until something in the light's reach changes.

The cascades also miss detail smaller than their texels (centimetres near the camera,
metres far away): an object resting on the ground, a hand on a table, grass. Screen-space
contact shadows fill that gap from the depth buffer.

## How others do it

### Atlases, resolution and budgets

- **id Tech 6/7** (Doom 2016, Doom Eternal; Sousa, Geffroy et al., SIGGRAPH 2016 and 2020):
  - all local lights' shadows in one large depth atlas (8k² in Doom 2016);
  - resolution per light from its screen coverage and distance, in power-of-two steps;
  - static lights and static geometry cached: a light's tiles are redrawn only when
    something in it changed; a per-frame update budget spreads redraws over frames;
  - point lights as six cube faces in the atlas.
- **Frostbite** (Battlefield; "Lighting the World", Coffin; "Physically Based Shading in
  Frostbite", 2014): a shadow atlas for local lights with a quadtree allocator; lights
  scored by screen size, the least important dropping resolution first, then their
  shadow; static shadows cached.
- **Unreal Engine 4 "cached shadow maps"** (documentation, 4.9+): for movable point and
  spot lights that do not move, the depth of the static primitives is cached; each frame
  the cache is copied and only movable primitives are drawn on top. Memory: a second map
  per light. Invalidation when the light moves or a static primitive changes.
- **Unreal Engine 5 virtual shadow maps** (documentation; Fortnite tech blog): a 16k²
  virtual map per light (a clipmap for the sun), 128² pages allocated where receivers on
  screen need them, cached between frames, invalidated per page by moving casters
  (instance bounds) or the light. Needs Nanite-class GPU-driven rendering and page tables;
  the roadmap has virtual shadow maps after meshlets (CLAUDE.md §17), and the atlas built
  here is the step before.
- **Unity HDRP** (documentation 16.0): one atlas for punctual lights (4096² by default),
  resolution per light with "dynamic rescale" by screen area, update modes every frame /
  on enable / on demand, "mixed cached shadows" (static casters cached, dynamic drawn each
  frame), three filter qualities.
- **Godot 4** (documentation, stable): a positional shadow atlas in four quadrants of
  4/4/16/64 slots, lights get a slot size by screen size; "if neither [the slot size nor
  anything in the light] changed, nothing is done, and the shadow is left untouched";
  omni lights as cube (default) or dual paraboloid.

### Point lights: cube, dual paraboloid, tetrahedron

| | views | texels for 360° at the same density | caveats |
|---|---:|---:|---|
| Cube (6 × 90°) | 6 | 6 r² | each caster drawn in 1–3 faces after per-face culling; seams need a border |
| Dual paraboloid (Brabec, Annen, Seidel 2002) | 2 | 2 r² (but uneven: 4× density change) | non-linear projection in the vertex shader: straight edges bend, needs tessellated casters; seam at the equator |
| Tetrahedron (Liao, GPU Pro 360 Shadows, 2018) | 4 | ~2.4 r² in two triangles per square | 109.5° frusta (stretched texels near corners), stencil or clip planes to keep triangles in their face, more complex face selection |

- The cube is the only one whose projection is an ordinary perspective (exact for any
  geometry) and whose views cull independently (a caster beside the light touches one
  face). Godot keeps it as the default for accuracy; id Tech and Unity use it.
- Lookup cost: with the face picked by the major axis and the projection written out (no
  matrix: the other two axes over the major one, depth A + B / major), a cube lookup costs
  a handful of ALU operations and one per-pixel load (the face's atlas rectangle).
- Patents: NVIDIA's US 8,803,879 B1 "Omnidirectional shadow texture mapping" (cube shadow
  maps with face selection; priority 2002) expired in July 2022 (checked on Google
  Patents). No patent found on dual-paraboloid or tetrahedron shadow mapping (academic).

### Filtering

- **PCF** with hardware comparison: one bilinear compare is a 2 × 2 PCF; four of them half a
  texel apart give a 3 × 3 tent (the approach of many engines and MJP's samples).
- **Vogel / Poisson disc PCF** (Unity, Godot "soft" qualities): N taps on a rotated disc,
  per-pixel rotation from noise; good with TAA, costs N taps (8–16).
- **Receiver-plane depth bias** (Isidoro, GDC 2006): derivatives of the shadow coordinates
  give a per-tap depth slope, for wide kernels; for a 3 × 3 kernel the normal offset
  (Holbert 2011) plus a slope-scaled rasterizer bias suffices.
- **PCSS** (Fernando 2005): blocker search + variable kernel, 16–32 + 16–64 taps per
  lookup. Too expensive per local light on the iGPU (the measured cost of four taps is
  already ~2.4 ms in the 7-light Sponza scene below). No patent found.

### Static / dynamic split

- UE4 cached shadow maps and Unity's mixed cached shadows keep the static casters' depth
  and redraw only dynamic casters over a copy each frame. That needs to know which
  instances are static, and costs a second tile and a copy per light.
- **Patent:** Warner Bros Entertainment, WO2021021755A1 / US 11,908,062 B2 / US
  12,315,068 B2 ("Efficient real-time shadow rendering", priority 2019-07-26): per-texel
  filtering of a static and a dynamic shadow map selected by a dynamic mask. Its claims
  are narrower than the general copy-and-draw scheme (which UE4 shipped in 2015), but the
  split is left out here and needs the team's review before it is built (see ADR 0071).

### GPU-driven culling per shadow view

- Wihlidal (GDC 2016) and Haar and Aaltonen (SIGGRAPH 2015): the same compute culling as
  the camera, run per shadow view, appending survivors to per-view indirect command lists.
- Here: the forward renderer already culls per cascade into per-cascade lists (ADR 0044);
  local views do the same with one more entry point (one thread per instance and view, the
  view's six frustum planes extracted from its matrix, LOD chosen from the light).

## Contact shadows

- **Bend Studio, screen-space shadows** (Days Gone, 2019; Graham Aldridge, "Inside Bend:
  Screen Space Shadows" and SIGGRAPH 2023 talk; code `bend_sss_cpu.h`, `bend_sss_gpu.h`,
  Copyright 2023 Sony Interactive Entertainment, **Apache-2.0**, checked 2026-10-01 at
  bendstudio.com/blog/inside-bend-screen-space-shadows):
  - the screen is walked in lines towards the light's projected point; a 64-thread wave
    owns 64 pixels of a line and reads the next depths along it into shared memory,
    perspective-corrected so every ray to the light is parallel there;
  - each thread then compares its own depth with the next 60 samples of the line: a long
    shadow for (60 / 64 + 2) × 2 = 4 depth reads per pixel;
  - the first 4 samples may shadow fully (contact), the rest average in four groups
    (softens single-pixel occluders), the last 8 fade out;
  - 1–8 dispatches depending on where the light's point is (CPU code splits the screen
    into quadrants around it).
  - Apache-2.0 carries a patent grant from the contributor for the contributed code.
- **Per-pixel ray march** (Unreal's contact shadows, Panos Karabelas' blog, many engines):
  N samples along the ray towards the light in world space, projected, compared with the
  depth within a thickness. Cost grows with N per pixel; long shadows need many samples.

Measured here (Sponza, 1920 × 1080, depth prepass on; scene_bench `--contact`):

| | RTX 4060 Ti | Radeon iGPU |
|---|---:|---:|
| Bend (60 samples) | 115 µs | 2.62 ms |
| Ray march (16 samples, 0.5 m) | 172 µs | 6.36 ms |

Bend's method is 1.5× (RTX) and 2.4× (Radeon) faster for shadows four times longer in
pixels; the march was dropped.

Tuning found on our depth buffer (conventional, not reversed, D32 float): Bend's
recommended surface thickness 0.005 left solid casters' shadows faint; 0.01 darkens 85 % of
the analytic contact region of a box with no false shadows. Distant flat ground at grazing
angles to the lines shadowed itself in stripes until edge pixels were ignored
(`IgnoreEdgePixels`) and Bend's precision offset was applied.

A screen-space shadow sees an occluder only where the ray crosses its visible surface:
it works best with the sun behind or beside casters; rays passing behind a thick caster's
front face stay lit (the shadow map covers those).

## What we take

1. One persistent **D16 atlas** (4096², 32 MiB) of power-of-two tiles from a **buddy
   (quadtree) allocator**; resolution per light from its projected diameter in pixels
   (× 0.5 texels per pixel, halved for cube faces), 64–1024 texels, all lights stepping
   down together when they would not fit in three quarters of the atlas; hysteresis
   (keep the tile while the wish stays within (⅓, 1.5×]).
2. **Cache:** redraw a light only when it changed (position, range, cone) or an instance
   whose box before or after its change touches the light's sphere was added, moved,
   edited, removed, or re-posed (skinned); the GPU scene's upload provides the changed
   boxes; a draw list is compared with the previous one by index.
3. **Budget:** out-of-date shadows are redrawn oldest and largest first, up to 12 views a
   frame; lights seen for the first time are drawn regardless.
4. **Eviction:** lights out of view keep their tiles until space is needed (least recently
   seen first).
5. **Spot = one view, point = six cube faces** with a 2-texel border; the face projected
   without a matrix in the lookup.
6. **3 × 3 tent PCF** (four bilinear compares), normal offset of a texel at the receiver's
   depth, slope-scaled rasterizer bias.
7. **GPU-driven culling per view** in a compute entry point of its own.
8. **Contact shadows:** Bend's method, as an R8 mask from the depth after the prepass, on
   the async compute queue where that is the default; off by default (2.6 ms on the iGPU).
9. **Register rule:** shading that looks shadows up or reads the mask is in separate entry
   points, used only by views that need them; the others are unchanged (VGPRs on the
   Radeon 48/64/64/80 before and after).

## What we leave for later

- Static/dynamic split (patent review first).
- Tetrahedron mapping for point lights (fewer views, measured only if cube redraws become
  the bottleneck).
- Softer filters (Vogel disc with TAA, PCSS) as a quality option in their own entry points.
- A screen-space shadow mask for local lights (shadows resolved per pixel in a compute pass
  after the prepass, the forward pass reading a mask channel): it would move the lookup's
  registers out of the light loop, which costs +12 % forward on the iGPU with 16 lights.
- Virtual shadow maps after meshlets (CLAUDE.md §17).

## Sources

- Tiago Sousa, Jean Geffroy, "The Devil is in the Details: idTech 666", SIGGRAPH 2016 Advances in Real-Time Rendering
- Jean Geffroy, Axel Gneiting, Yixin Wang, "Rendering the Hellscape of Doom Eternal", SIGGRAPH 2020 Advances
- Christina Coffin, "SPU-based Deferred Shading in Battlefield 3", GDC 2011; Sébastien Lagarde, Charles de Rousiers, "Moving Frostbite to PBR", SIGGRAPH 2014
- Epic Games, "Shadow Map Caching" (UE 4.27 documentation) and "Virtual Shadow Maps in Unreal Engine" (UE5 documentation), checked 2026-10-01
- Unity, "Shadows in the High Definition Render Pipeline", HDRP 16.0 documentation, checked 2026-10-01: https://docs.unity3d.com/Packages/com.unity.render-pipelines.high-definition@16.0/manual/Shadows-in-HDRP.html
- Godot, "3D lights and shadows", stable documentation, checked 2026-10-01 (MIT): https://docs.godotengine.org/en/stable/tutorials/3d/lights_and_shadows.html
- Stefan Brabec, Thomas Annen, Hans-Peter Seidel, "Shadow Mapping for Hemispherical and Omnidirectional Light Sources", CGI 2002
- Hung-Chien Liao, "Shadow Mapping for Omnidirectional Light Using Tetrahedron Mapping", GPU Pro 360 Guide to Shadows, 2018
- Randima Fernando, "Percentage-Closer Soft Shadows", SIGGRAPH 2005 sketch
- John Isidoro, "Shadow Mapping: GPU-based Tips and Techniques", GDC 2006
- Daniel Holbert, "Saying Goodbye to Shadow Acne" (normal offset), GDC 2011
- MJP, "A Sampling of Shadow Techniques": https://therealmjp.github.io/posts/shadow-maps/
- Graham Wihlidal, "Optimizing the Graphics Pipeline with Compute", GDC 2016
- Graham Aldridge (Bend Studio), "Inside Bend: Screen Space Shadows", code `bend_sss_cpu.h` / `bend_sss_gpu.h` (Apache-2.0), checked 2026-10-01: https://www.bendstudio.com/blog/inside-bend-screen-space-shadows/
- Panos Karabelas, "Screen space shadows": https://panoskarabelas.com/blog/posts/screen_space_shadows/
- Patents checked 2026-10-01 on Google Patents: US 8,803,879 B1 (NVIDIA, omnidirectional shadow texture mapping, expired 2022-07-10); WO2021021755A1, US 11,908,062 B2, US 12,315,068 B2 (Warner Bros Entertainment, efficient real-time shadow rendering: static and dynamic shadow maps combined by a dynamic mask)
