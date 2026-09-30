# Directional light shadows: cascaded shadow maps

Checked 2026-09-30. Sources at the end. No code was copied; the techniques come from the
papers, talks and documentation, and the engines' code was read for their defaults.

## Why

The engine has one 2048² shadow map fitted to the bounding sphere of the whole scene
(ADR 0014):

- **Sponza** (about 30 m across) gets about 1.5 cm per texel, which is acceptable.
- **A 1 km outdoor level** gets 50 cm per texel everywhere, including one metre in front
  of the camera. Shadows of a character or a chair become blobs.
- **The shadow pass draws every instance of the scene,** visible from the camera or not.

Receivers near the camera need small texels; far receivers can take large ones. That is
what cascades provide.

## How others do it

- **Parallel-split shadow maps** (Zhang et al. 2006):
  - the view frustum is cut along its depth into N slices, each with its own map;
  - the "practical" split interpolates between logarithmic splits (ideal texel density,
    tiny first slice) and uniform ones with a λ:
    s_i = λ · n (f / n)^(i/N) + (1 − λ) · (n + (f − n) · i/N).
- **Stable cascades** (Valient, ShaderX6, Killzone 2; Microsoft's "Cascaded Shadow Maps"):
  - a map fitted tightly to a slice changes size and position whenever the camera
    rotates, and shadow edges crawl;
  - instead, fit each slice's *bounding sphere*: its radius does not change with
    rotation;
  - move the map only in whole texels ("snap to texel"): translation then never
    resamples edges.
  - The cost is resolution: the sphere is larger than the slice.
- **Cascade selection** (Microsoft):
  - **interval-based:** pick by view depth; slightly cheaper;
  - **map-based:** pick the first cascade whose map contains the point; uses finer
    cascades more often when maps are spheres.
  - Blending over a band at the boundary hides the change of resolution. It costs a
    second lookup in the band.
- **Bevy** (MIT/Apache, `bevy_light::cascade`, main branch):
  - 4 cascades; the first ends at 10 m, the last at 150 m (chosen "to be similar to Unity,
    Unreal, and Godot"); 20% overlap between cascades for blending;
  - an axis-aligned fit in light space with the diameter ceiled to an integer and the
    near plane center snapped to whole texels.
- **Godot** (MIT, documentation, stable):
  - PSSM with 1, 2 or 4 splits, relative to the shadow max distance;
  - "Blend Splits" trades detail and performance for smooth transitions;
  - "Pancake Size" keeps casters in front of the near plane;
  - recommends normal bias over depth bias (less peter-panning);
  - soft filter quality levels from very low to ultra.
- **Filament** (Apache-2.0, `LightManager.h`, main at 14fae47):
  - 1 cascade by default, split positions 0.125, 0.25, 0.5 when enabled; uniform,
    logarithmic and practical (λ) helpers;
  - bias: `constantBias` 1 mm in world units, plus `normalBias` 1.0 times the sampling
    error, along the normal;
  - `stable` trades resolution for stability; LiSPSM otherwise.
- **MJP, "A Sampling of Shadow Techniques"** (blog and sample, 2013–2015):
  - stabilization by sphere and texel snapping; SDSM partitions;
  - fixed-size PCF (a 7 × 7 GatherCmp kernel is ~0.4 ms at 1080p on his hardware),
    grid and random-disc PCF;
  - EVSM and moment shadow maps: filterable and soft, but they cost memory, need
    high-precision storage and still leak light;
  - bias: manual, normal offset (cheap, shifts the shadow), receiver plane (slope-aware,
    unstable in degenerate cases);
  - depth clamping ("pancaking") so casters in front of the near plane still cast;
  - GPU-driven cascade setup and caster culling, faster than the CPU path once it
    removes the stall.
- **Sample distribution shadow maps** (Lauritzen, Salvi, Lefohn, I3D 2011): fit the
  partitions to the depth range actually on screen, reduced on the GPU. They are much
  tighter than a fixed distance, but lose stability unless quantized.
- **Normal offset shadows** (Holbert, GDC 2011): offset the receiver along its normal by
  a multiple of the texel's world size. It removes acne without the peter-panning of a
  large depth bias.
- **Virtual shadow maps** (Unreal Engine 5, 2021):
  - a 16k² paged map per light; only pages that pixels need are rendered;
  - pages are cached across frames;
  - nanite clusters make re-rendering cheap.
  - The current state of the art for large worlds; it needs meshlet rendering first.

## Conclusions

1. **Cascades:**
   - 1–4, default 4;
   - practical splits with λ (default 0.75, which puts the first split near 10 m at 150 m,
     like Bevy's defaults);
   - covering at most a shadow distance, and no further than the scene's bounds reach.
2. **Stable fitting:**
   - a bounding sphere per slice, its radius rounded up;
   - one light view shared by all cascades, which differ only by a scale and an offset;
   - centers snapped to whole texels;
   - depth covers the scene's bounds from the light to the far side of the sphere, so
     casters in front of the slice still cast without depth clamping.
3. **One atlas** (2 × 2 tiles for 3–4 cascades), rendered in one pass with a viewport per
   cascade. It is one transient texture and one barrier.
4. **Caster culling on the GPU, per cascade,** in the same compute shader as camera
   culling: each instance's box in light space against each cascade's square and far
   depth. Visible casters are appended to per-cascade indirect commands.
5. **Map-based selection** with a margin for the filter kernel; blending over a band at
   each boundary; fading out at the edge of the last cascade.
6. **Bias:**
   - the existing slope-scaled depth bias of the shadow pipeline;
   - a normal offset scaled by each cascade's texel size (Holbert, as in Filament).
7. **Later, from measurements:**
   - SDSM-style splits from the depth pyramid that occlusion culling already builds;
   - caching of far cascades;
   - small-caster culling in far cascades;
   - softer filters (PCSS, Gather-based PCF);
   - virtual shadow maps after meshlets.

## Sources

- Zhang, Sun, Xu, Lau, "Parallel-Split Shadow Maps for Large-scale Virtual Environments", VRCIA 2006
- Valient, "Stable Rendering of Cascaded Shadow Maps", ShaderX6 (2008)
- Microsoft, "Cascaded Shadow Maps" (https://learn.microsoft.com/en-us/windows/win32/dxtecharts/cascaded-shadow-maps) and "Common Techniques to Improve Shadow Depth Maps" (https://learn.microsoft.com/en-us/windows/win32/dxtecharts/common-techniques-to-improve-shadow-depth-maps)
- Bevy, `crates/bevy_light/src/cascade.rs`, main branch, checked 2026-09-30 (MIT OR Apache-2.0): https://github.com/bevyengine/bevy/blob/main/crates/bevy_light/src/cascade.rs
- Godot documentation, "3D lights and shadows", stable, checked 2026-09-30 (MIT): https://docs.godotengine.org/en/stable/tutorials/3d/lights_and_shadows.html
- Filament, `filament/include/filament/LightManager.h` and `filament/src/ShadowMapManager.cpp`, main at 14fae473, checked 2026-09-30 (Apache-2.0): https://github.com/google/filament
- MJP, "A Sampling of Shadow Techniques" (https://therealmjp.github.io/posts/shadow-maps/)
- Lauritzen, Salvi, Lefohn, "Sample Distribution Shadow Maps", I3D 2011
- Holbert, "Saying Goodbye to Shadow Acne" / normal offset shadows, GDC 2011 poster
- Isidoro, "Shadow Mapping: GPU-based Tips and Techniques", GDC 2006 (receiver plane depth bias)
- Fernando, "Percentage-Closer Soft Shadows", SIGGRAPH 2005 sketch
- Peters, Klein, "Moment Shadow Mapping", I3D 2015
- Epic Games, "Virtual Shadow Maps", Unreal Engine 5 documentation
