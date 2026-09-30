# 0044. Cascaded shadow maps: stable cascades in an atlas, casters culled per cascade

- Status: Accepted
- Date: 2026-09-30
- Supersedes the shadow part of [ADR 0014](0014-pbr-and-shadows.md) (one map fitted to the
  scene)
- Builds on: [ADR 0034](0034-gpu-culling-and-indirect-draws.md) (GPU culling and indirect
  draws), [ADR 0043](0043-motion-vectors-and-taau.md) (TAA resolves dithering)
- Research: [`docs/research/shadows.md`](../research/shadows.md)

## Context

ADR 0014 fitted one 2048² shadow map to the bounding sphere of the whole scene:

- **Texel size grows with the scene, not with the distance from the camera.** Sponza gets
  1.5 cm per texel; a 400 m level gets 20 cm right in front of the camera. A thin pole's
  shadow disappears.
- **The shadow pass drew every instance of the scene,** whether its shadow could reach
  the view or not.

## Options

**Resolution where it is needed:**

1. **Cascaded shadow maps (chosen):**
   - the view is split along its depth, each slice with its own map;
   - the standard in Unity, Unreal, Godot, Bevy and Filament.
2. **Sample distribution shadow maps:** partitions fitted to the depths on screen, from
   the depth pyramid occlusion culling already builds. Much tighter, but unstable unless
   quantized; kept as the next step for small scenes.
3. **Virtual shadow maps** (Unreal 5): a paged 16k² map. The state of the art for large
   worlds, but it needs meshlet rendering first.

**Stability:**

- a. A tight fit to each slice: the most resolution, and edges crawl whenever the camera
  turns.
- b. **A bounding sphere per slice and whole-texel snapping (chosen)** (Valient): the
  radius does not change when the camera turns, and moving the camera moves the map by
  whole texels.

**Cascade selection:**

- a. By view depth (interval-based): simple, but uses a finer cascade less often.
- b. **By position in the maps (map-based, chosen):** the finest cascade whose square,
  with room for the filter, and whose depth range hold the point.

**Transitions between cascades,** measured (Sponza, 1080p, forward µs, textured):

| | RTX | Radeon |
|---|---:|---:|
| hard (no band) | 311 | 7,881 |
| blending two lookups over a band of 10% | 343 | 8,277 |
| blending over 5% | 318 | 8,201 |
| **dithering over 10% (chosen)** | 317 | 7,778 |

A second filtered lookup costs registers even where it never runs. Dithering picks the
next cascade with a probability rising across the band: one lookup per pixel, and TAA's
jitter moves the noise, so the transition resolves smooth. Without TAA it stays a
static, stable dither pattern.

**Light-space position:**

- per pixel (one matrix product in the fragment shader), or
- **per vertex, as two varyings (chosen).**

| Sponza, forward µs | RTX | Radeon |
|---|---:|---:|
| per pixel | 309 | 9,034 |
| **per vertex** | 322 | 8,298 |

Taken for the iGPU: −8% there, +4% (13 µs) on the RTX.

## Decision

- **`engine_render::shadow`:**
  - `fit_cascades` splits the view into 1–4 slices with practical splits (λ = 0.75);
  - shadows reach the `distance` setting (150 m), and never beyond the scene's bounds or
    the camera's far plane;
  - each slice gets a bounding sphere, grown by the filter's margin and half a texel of
    snapping, its radius rounded up to 1/64 of its power of two, centered on whole
    texels;
  - all cascades share one light view (a rotation) and differ by a diagonal scale and an
    offset;
  - depth covers the scene's bounds from the light's side to the far side of the slice,
    so casters in front of a slice cast into it without depth clamping.
- **`ShadowSettings`** (moved to `shadow.rs`, re-exported from `forward`):

  | Field | Default | Meaning |
  |---|---|---|
  | `resolution` | 1024 | texels per cascade; the 2 × 2 atlas is 2048², the old map's size |
  | `format` | D32 | |
  | `cascades` | 4 | 1–4 |
  | `distance` | 150 | world units |
  | `split_lambda` | 0.75 | 0 uniform … 1 logarithmic |
  | `blend` | 0.1 | dithered band at each cascade's edge |
  | `normal_bias` | 1.0 | receiver offset along its normal, in texels of its cascade |

- **Caster culling on the GPU, per cascade:**
  - the culling shader's shadow phase transforms each instance's box into light view
    space;
  - it tests the box against each cascade's square and far depth, and appends the
    casters to that cascade's indirect commands and caster list.
- **One atlas, one pass:** "shadow" draws each cascade into its tile with its own
  viewport; the existing slope-scaled depth bias stays.
- **Receivers:**
  - the vertex shader passes the light view position and normal;
  - the fragment shader picks the cascade (map-based) and offsets the receiver along its
    normal by `normal_bias` texels of that cascade (Holbert);
  - then 3 × 3 PCF, and the dithered transition.
  - The last cascade dithers out only when shadows end at the distance setting, before
    the scene does.
- `draw_bench` and `scene_bench` take `--cascades`, `--shadow-res`; `scene_bench` also
  `--shadow-blend`. "shadow cull" counts in their shadow column.

## Measurements

ref-1, release, 1920 × 1080, GPU µs, medians. "Before" is `feat/taau` (17daf47), one
2048² map over the whole scene.

**Correctness** (`tests/shadows.rs`, under synchronization validation, 0 errors on both
GPUs): a 400 m ground with boxes. Every ground pixel is checked against analytic
shadows; pixels within 1.5 cm + 0.4% of their distance of an edge are not judged.

| judged pixels: 65,755 | 4 cascades of 1024² | 1 map of 1024² over the same distance |
|---|---:|---:|
| wrong | **0** | 79 |
| wrong within 12 m | **0** | 30 (a 15 cm pole's shadow lost) |
| shadow of a box 90 m away | 19 / 19 | 18 / 19 |
| shadow of a tower outside the view | 934 / 934 | 903 / 934 |

No lit pixel came out dark (no acne), and no shadowed pixel came out lit (no gap under
casters). The CPU tests check:

- that splits grow and texels grow;
- that every point of a slice is held by its cascade;
- that turning the camera never changes a texel's size;
- that moving it moves maps by whole texels;
- that every visible point within reach is held;
- that a degenerate camera gives no cascades.

**Cubes** (`draw_bench`; RTX up to 100k, Radeon up to 10k), frustum culling:

| instances, view | RTX shadow | RTX forward | Radeon shadow | Radeon forward |
|---|---:|---:|---:|---:|
| 1k, overview | 14 → 14 | 26 → 34 | 100 → 93 | 658 → 716 |
| 1k, city blocks | 22 → 28 | 84 → 98 | 143 → 166 | 2,038 → 2,555 |
| 10k, overview | 43 → 31 | 51 → 68 | 449 → 592 | 1,306 → 1,390 |
| 10k, city blocks | 66 → 62 | 98 → 115 | 563 → 636 | 2,243 → 2,819 |
| 100k, overview | 263 → **18** | 269 → 261 | – | – |
| 100k, inside the field | 263 → **58** | 65 → 76 | – | – |
| 100k, city blocks | 317 → **83** | 119 → 139 | – | – |

With culling off (a debug mode), every cascade draws every instance: 100k costs 953 µs
of shadow on the RTX.

**Scenes** (`scene_bench`, textured / factors only):

| | shadow | forward |
|---|---:|---:|
| Sponza, RTX | 70 → 159 | 282 / 166 → 317 / 211 |
| Sponza, Radeon | 763 → 2,292 | 6,879 / 3,609 → 7,778 / 4,622 |
| FlightHelmet, RTX | 36 → 21 | 61 / 43 → 73 / 56 |
| FlightHelmet, Radeon | 286 → 468 | 1,782 / 1,158 → 1,991 / 1,292 |

**Presets** (Sponza textured, shadow / forward):

| | RTX | Radeon |
|---|---:|---:|
| 2 cascades of 1024² | 76 / 303 | 1,197 / 7,519 |
| 4 of 1024² (default) | 159 / 317 | 2,292 / 7,778 |
| 4 of 2048² | 353 / 333 | 4,103 / 8,057 |

Resolution alone (one cascade, Sponza, Radeon forward) is 7,981 µs at 512² and 8,087 µs
at 4096²: the forward cost is the shader, not the map's size.

- **Large scenes get faster and sharper.** Culling casters per cascade pays for four
  cascades many times over at 100k instances (RTX shadow 263 → 18–83 µs).
- **A small interior pays four passes over its geometry.** Sponza sits inside every
  cascade, so its shadow pass costs 2.3× on the RTX and 3× on the Radeon iGPU.
- **The shadow lookup costs 10–30% of a cheap forward pass.** That is the case of cubes
  and factor-only materials. On textured Sponza it is +12% (RTX) and +13% (Radeon).
  - The selection loop, the normal offset and two more varyings are the price.
  - Tried and dropped:
    - per-pixel light transform: +8% on the Radeon;
    - two blended lookups: +6%;
    - selecting whole cascades by selects without a loop: +37% on the Radeon, from
      registers.

## Consequences

- **Every renderer gets cascades by default** with the old map's memory (2048² D32, 16 MB
  transient). Games tune `ShadowSettings` per quality preset; the dev guide gives the
  numbers above.
- **For integrated GPUs, 2 cascades** keep most of the gain near the camera at half the
  shadow pass.
- **Next, from measurements:**
  - fit cascades to the part of each slice the scene occupies (SDSM-like, from the depth
    pyramid) for small scenes, keeping stability by quantizing;
  - cache far cascades and re-render them every few frames;
  - cull casters smaller than a texel in far cascades;
  - interval-based selection, if a register analysis (Radeon GPU Analyzer) shows the
    loop is what costs on the iGPU;
  - softer filters (PCSS, Gather-based PCF) as a quality setting.
- **Virtual shadow maps** become the target after meshlets (ADR 0034's follow-up).
