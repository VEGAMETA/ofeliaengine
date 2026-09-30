# 0054. Transparency: sorted blended instances in a forward pass, a reactive mask for TAAU

- Status: Accepted
- Date: 2026-09-30
- Research: [docs/research/transparency.md](../research/transparency.md)
- Related: [ADR 0034](0034-gpu-culling-and-indirect-draws.md) (GPU culling, batches),
  [ADR 0035](0035-occlusion-culling.md) (two-phase occlusion),
  [ADR 0043](0043-motion-vectors-and-taau.md) (TAAU),
  [ADR 0053](0053-mesh-lods.md) (detail levels)

## Context

The forward renderer draws only opaque and alpha-tested surfaces. glTF's `BLEND` materials
(glass, water, visors, smoke cards) were drawn opaque. Transparency was the team's first
priority (2026-09-30), before shader caches, streaming, AO and SSS.

Three things need to be decided:

- **how the layers are ordered:** "over" blending is not commutative;
- **how that order survives GPU culling:** the culling pass appends instances to commands
  in whatever order its threads run;
- **what TAAU does with them:** a transparent surface has no motion vector of its own
  (the pixel carries the opaque surface behind it), so its history is reprojected wrongly
  and it smears or ghosts.

## Options

### Ordering the layers

1. **Sorted per object, blended back to front** (chosen): exact for layers that do not
   intersect, one pass, the usual shading. Wrong where objects intersect or one contains
   another, and within one mesh triangles blend in index order.
2. **Weighted blended OIT** (McGuire and Bavoil, 2013): no sorting, two extra targets and
   a composite pass. The result approximates the order by depth weights: wrong for
   opaque-ish layers (alpha above about 0.7), and the weights need tuning per scene
   scale.
3. **Per-pixel linked lists or a k-buffer:** exact. Memory per pixel is unbounded (or
   capped at k layers), with a sort per pixel; too expensive for the Radeon iGPU at
   1080p.
4. **Depth peeling:** exact, one geometry pass per layer.
5. **Stochastic transparency:** order independent through coverage masks and TAA. Noisy
   at low alpha, and it depends on the temporal resolve being on.

### Keeping the order through GPU culling

1. **One batch, and one draw command, per blended instance** (chosen): the commands are
   already in sorted order; culling only zeroes the instance count of a hidden one. The
   cost is per instance on the CPU (a batch record, a command per list) and in the
   indirect draw (one command each).
2. **A GPU sort of the visible blended instances after culling** (a compute radix sort
   by depth): cheaper per instance at thousands of instances, but another pass and a
   sort per frame. Worth it for particles, which are their own system.

### TAAU

1. **A reactive mask** (chosen; the idea of AMD FSR 2's reactive mask, from its public
   documentation): the transparent pass also writes its combined coverage, and the resolve
   lets history count less where transparent layers cover the pixel.
2. **Drawing transparent surfaces after the resolve**, at the output resolution (Unreal's
   "separate translucency"): no ghosting, but no antialiasing for them, a full-resolution
   depth needed, and full-resolution shading.
3. **Nothing:** transparent surfaces over moving backgrounds ghost.

## Numbers

`draw_bench`: 10,000 cubes, frustum culling, 1920×1080; `--blended` makes every material
blended with alpha 0.5. Values in µs.

| | RTX 4060 Ti | Radeon iGPU |
|---|---:|---:|
| add_passes (CPU), opaque | 84 | 64 |
| **add_passes (CPU), blended** | **452–460** | **442–444** |
| culling, opaque | 4.7–4.8 | 22.9–25.0 |
| **culling, blended** | **3.3–4.1** | **82.7–93.2** |
| forward + transparent, opaque (overview / inside) | 69 / 77 | 1,424 / 1,585 |
| **forward + transparent, blended (overview / inside)** | **92 / 127** | **2,057 / 2,124** |

- Per blended instance the CPU pays about 37 ns: a batch, a sort key, a command in
  every list (both occlusion phases and each shadow cascade).
- Writing the batch table and the commands in memory order took the commands from
  480 to 144 µs of the blended 10k frame. The ring may be write-combined memory, where
  scattered writes are slow.
- Opaque scenes: add_passes for 100k cubes 808 → 834 µs (+3%), the third bit of a
  material's pipeline state; GPU times unchanged. Sponza: shadow 174 µs, forward 341 µs,
  as before.

## Decision

- **`AlphaMode::Blend`:** base color alpha is coverage. The whole shading scales with it
  (glTF's `BLEND`), premultiplied, drawn with `Blend::Premultiplied`. The material kind
  gets a `blended` bit and the pipeline state a `BLENDED` bit (16 states).
- **Sorting:** batches are sorted by state and distance to the camera from the box center;
  blended ones by the inverted distance, so back to front. A blended draw never extends a
  batch: each is its own batch and its own command, in sorted order.
- **Culling:**
  - blended instances cast no shadows;
  - they are skipped in the early phase of occlusion culling and tested in the last
    phase against the pyramid of the opaque depth;
  - the batch record carries the flag in `lods` bit 8.
- **The transparent pass:** after the sky, before any temporal resolve.
  - Color load/store, depth tested (LESS), not written.
  - Lit like opaque surfaces: sun and cascades, clustered local lights, the environment.
  - Draws the last phase's commands of the blended states.
- **The reactive mask:**
  - `R8_UNORM`, written only when the view is temporal and something is blended;
  - blended the same way (premultiplied), so it holds `1 − Π(1 − α)`;
  - `MotionOutput::reactive`;
  - TAAU lowers the history cap towards one frame's weight by the coverage:
    `cap = lerp(cap, 1, coverage)`. A fully covered pixel then blends history and the new
    frame half and half.
- Order-independent transparency is deferred to particles, with their own sorting on the
  GPU.

## Consequences

- glTF `BLEND` materials render as intended; scene_bench imports them as blended.
- Transparent layers stay antialiased and upsampled with the rest, without the ghosting
  of wrong motion vectors, at the cost of more temporal noise where they cover.
- Limits to know:
  - objects that intersect, or that contain one another, blend in the wrong order in
    places;
  - self-overlapping transparent meshes (a closed double-sided glass) depend on index
    order;
  - transparent surfaces neither cast shadows nor appear in the occlusion pyramid;
  - no refraction or screen-space distortion yet.
- Thousands of small transparent objects cost CPU per instance (~37 ns each): that is
  the particle system's job, with a GPU sort.
- Next:
  - a GPU sort of blended instances if scenes carry thousands of them;
  - shadows from transparent surfaces (colored or stochastic);
  - refraction via a copy of the opaque color;
  - a reactive mask for animated textures (the same target, written by opaque materials
    that ask for it).
