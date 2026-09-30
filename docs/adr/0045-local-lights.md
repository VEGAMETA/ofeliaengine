# 0045. Local lights: z-bins and tile bitmasks

- Status: Accepted
- Date: 2026-09-30
- Builds on: [ADR 0040](0040-materials-and-textures.md) (the forward pass and its BRDF),
  [ADR 0044](0044-cascaded-shadow-maps.md) (the frame constants it extends)
- Research: [`docs/research/many-lights.md`](../research/many-lights.md)

## Context

The forward renderer shades one directional light. Interiors, night streets, muzzle
flashes and effects need hundreds to thousands of point and spot lights. Every light for
every pixel costs lights × pixels:

- 1,024 lights over Sponza at 1080p take 7.4 ms of forward shading on the RTX 4060 Ti;
- the same take 297 ms on the Radeon iGPU.

## Options

1. **Tiled forward ("Forward+", Harada 2012):** a light list per screen tile, culled with
   the tile's depth bounds. It needs a depth prepass (ours is off by default, ADR 0042),
   and depth discontinuities make lists long.
2. **Clustered (Olsson 2012, Persson 2015, Doom 2016, Bevy):** lists per froxel (tiles ×
   exponential depth slices). It needs no prepass, but memory grows with froxels × lights
   per froxel, which calls for list limits or a compacted index buffer.
3. **Z-binning (Drobot 2017, Call of Duty) (chosen):**
   - lights sorted by view depth;
   - per depth bin, the lowest and highest index of the lights overlapping it;
   - per screen tile, one bit per light whose screen bounds cover it;
   - a pixel shades the lights in both.
   - No per-cell limit, memory tiles × lights / 8, depth resolved per pixel, and bit
     operations that suit a wave.

## Decision

- **API:**
  - `LocalLight { position, range, color, spot: Option<Spot { direction, inner, outer }> }`
    in `engine_render::lights`, re-exported from `forward`;
  - `ForwardView::lights: &[LocalLight]`;
  - `ForwardRenderer::set_light_culling(false)` evaluates every light for every pixel:
    the reference.
- **Shading:**
  - GGX / Smith / Schlick like the sun, through the same `surface_light` function;
  - inverse-square falloff windowed to zero at `range`, (1 − (d/r)⁴)² / d² (Karis 2013);
  - spot cones saturate(cos · scale + offset)² between the inner and outer angles.
- **CPU per frame** (`lights.rs`; scratch buffers, no allocation in steady state):
  - frustum culling of each light's sphere;
  - a sort by depth along the view axis;
  - 1,024 z-bins, linear up to the farthest light, as 16-bit index pairs, so 65,535
    lights at most;
  - each light's tile rectangle (16 × 16-pixel tiles) from the sphere's exact screen
    bounds, its projected quadric (Sigg et al. 2006), or the whole screen when the
    sphere reaches the plane of the eye.
- **GPU:**
  - "light tiles clear" and "light tiles": one group per light sets its bit, with an
    atomic OR, in every tile of its rectangle;
  - the forward shader reads its pixel's bin, walks the words between the bin's indices
    in its tile's mask, and iterates the OR of the wave's masks, so each light's data is
    one shared load.
- **Specialization constant `LOCAL_LIGHTS`:** the shading pipelines exist with and
  without the light loop. A frame without lights uses the latter, and the loop's
  registers cost nothing there. `GraphicsPipelineDesc::specialization` was added to
  `engine_rhi` for it.
- **Benchmark:** `scene_bench --lights N [--no-light-culling]` spreads N lights over the
  scene (every fourth a spot), and reports "light tiles" and the CPU `add_passes` time.

## Measurements

ref-1, release, Sponza textured, 1920 × 1080, 4 shadow cascades, GPU µs (medians).
Light ranges are 3–8% of the scene's size, so dozens of lights overlap each pixel.

| lights | RTX forward | RTX light tiles | Radeon forward | Radeon light tiles | CPU add_passes |
|---:|---:|---:|---:|---:|---:|
| 0 | 317 | – | 7,728 | – | 9–12 |
| 64 | 373 | 12 | 10,633 | 14 | 12–15 |
| 256 | 470 | 13 | 14,134 | 28 | 20–32 |
| 1,024 | 849 | 36 | 25,925 | 624 | 55–66 |
| 4,096 | 2,450 | 73 | 72,629 | 3,339 | 217–228 |

**Every light for every pixel** (`--no-light-culling`):

| lights | RTX | Radeon |
|---:|---:|---:|
| 256 | 1,957 (4.2× the culled) | 79,211 (5.6×) |
| 1,024 | 7,427 (8.7×) | 297,162 (11.5×) |

**Steps measured on the way** (forward / light tiles):

| step | RTX, 1,024 lights | RTX, 4,096 lights | Radeon, 4,096 lights |
|---|---:|---:|---:|
| tile pass: a thread per (tile, word) over 32 rectangles | 949 / 72 | 2,810 / 439 | 82,217 / 6,462 |
| tile pass: a group per light with atomic ORs | 948 / 45 | 2,809 / 93 | 82,251 / 4,615 |
| rectangles from the sphere's exact bounds instead of its cube's corners | 849 / 36 | 2,450 / 73 | 72,629 / 3,339 |

- **Without the specialization constant,** the light loop cost frames without lights
  10%: 317 → 349 µs on the RTX, 7,778 → 8,571 µs on the Radeon.
- **Correctness** (`tests/lights.rs`, under synchronization validation, both GPUs):
  - 602 point and spot lights, including ones behind the camera, around its near plane
    and far away;
  - culling gives the reference image to within 1e-4 relative (half-float rounding): 0
    channels beyond 2/1024;
  - the lights light 17% of the pixels.
- **CPU tests check:**
  - frustum culling and depth order;
  - that every light is inside the index range of every bin it overlaps;
  - that rectangles cover the projected sphere;
  - that the exact bounds are tight to 1% (perspective) and exact (orthographic);
  - spot cone attenuation.

## Consequences

- **Scenes with hundreds of lights are affordable on the RTX,** and culling makes them 4–12×
  cheaper everywhere. Most of the remaining cost is shading the lights that do reach a
  pixel.
- **Not done yet:**
  - shadows of local lights;
  - light units beyond "color × intensity" (photometric units with the exposure work);
  - spot cones bounded by spheres, loose for narrow cones;
  - the editor does not place lights yet.
- **Next, from measurements:**
  - cone bounds for spots;
  - conservative proxy rasterization (Drobot) for lights near the camera, which cover
    whole-screen rectangles;
  - on the iGPU, light tiles at 1,024+ lights (0.6–3.3 ms): per-word union rectangles
    or fewer atomics;
  - the same bins for decals and reflection probes.
