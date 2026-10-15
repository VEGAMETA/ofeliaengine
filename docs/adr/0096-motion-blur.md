# 0096. Motion blur: Guertin et al.'s reconstruction filter, on cinematic views only

- Status: Accepted
- Date: 2026-10-15
- Research: [docs/research/motion-blur.md](../research/motion-blur.md)
- Related:
  - [ADR 0067](0067-no-camera-motion-blur-in-gameplay.md): the team's decision of
    2026-10-01, which this follows;
  - [ADR 0095](0095-depth-of-field.md): the same place in the frame;
  - [ADR 0072](0072-frame-renderer.md): the frame renderer;
  - queue item 6a.

## Context

- **ADR 0067:** camera motion blur exists only on cinematic views (cutscenes,
  sequences), and a gameplay view has none to turn on. The full effect (camera and object
  motion) is built and tested; only its use is limited.
- **Object-only motion blur in gameplay** is a separate, open question.
- **The renderer has what the filter needs:**
  - motion vectors at render size (texture coordinates now minus last frame's, without
    jitter; background pixels carry the camera's motion);
  - the depth.

## Options

1. **Each pixel along its own motion vector** (Bevy). Cheap, but a fast object in front
   of a still background does not blur past its own silhouette.
2. **Tile max, neighbour max and a reconstruction gather** (McGuire et al. 2012, the
   method most engines ship). Objects streak over their surroundings, but each tile has a
   single direction: overlapping motions, tile edges, thin objects ghosting.
3. **Guertin, McGuire and Nowrouzezahrai 2014.** Option 2 made robust:
   - two sampling directions;
   - weights aware of the directions involved;
   - stochastic tile borders;
   - a normalized center weight;
   - a relative depth test.
4. **Scattered sprites per moving pixel.** Expensive; no need for it in cutscenes.

## Decision

**Option 3, written from the paper (no code taken).**

`engine_render::motion_blur::MotionBlur` has three compute passes over the image the tone
mapper reads, after depth of field:

1. **Tiles.**
   - Each pixel's velocity and linear depth go into one texel. The velocity is half its
     streak in pixels: the shutter's share of the frame's motion, clamped to [0.5, r].
   - The pass also finds each r × r tile's largest velocity. r is the longest blur: 0.037
     of the height by default, 40 pixels at 1080p as in the paper.
   - Neighbouring threads handle neighbouring pixels.
2. **Neighbours:** the largest of the 3 × 3 tiles, a diagonal tile only if its streak runs
   within 45° of the center tile.
3. **Gather**, N samples per pixel (19 by default):
   - **Directions:** the samples alternate between the neighbourhood's velocity and the
     pixel's own.
   - **Three weights**, each multiplied by its direction factor:
     - a sample blurring over the pixel;
     - the pixel's blur showing what is behind it;
     - both together.
   - **Depth:** a relative comparison, `clamp(2 − a / b)`.
   - **The center weight:** N / (40 ‖v‖).
   - **The sample's distance** is measured along the dominant velocity for both kinds of
     samples, the paper's T = t ‖v_max‖.
     - Measured along the pixel's own still velocity, the samples gathered at a still
       pixel all land on it, with the weight of sharp samples.
     - A streak 6 pixels past a moving square then dropped from 1.73 to 0.24.
   - **Jitter:**
     - the samples move by τ ψ / N of a spacing per pixel, with interleaved gradient noise
       standing in for the paper's Halton values;
     - the neighbourhood lookup crosses into the next tile near borders, with a
       probability falling from 1/2 to 0 at the tile's center.
   - The image is fixed per pixel: no temporal noise.

**`engine_frame`:**

- `ViewSettings::kind` replaces `cinematic: bool`.
  - `ViewKind::Gameplay` (the default) has no motion blur.
  - `ViewKind::Cinematic { motion_blur }` holds its settings, so a gameplay view cannot
    express them.
- A cinematic view with motion blur asks for motion vectors.
- The pass runs after depth of field and before bloom.
- `engine_runtime`'s `Camera` gets a `shutter`: a cinematic camera with a shutter above 0
  has motion blur.

`MotionBlurSettings`: `shutter` (0.5: 180°), `samples` (19), `max_radius` (0.037 of the
height).

## Measurements

ref-1, release, 1920 × 1080. The perf gate's `sponza_motion_blur`: Sponza walked through
at 7 cm a frame, a 180° shutter, 19 samples, every pixel moving.

| | RTX 4060 Ti | Radeon iGPU |
|---|---:|---:|
| tiles, neighbours, gather | 432 µs (72, 3, 356) | 15.7 ms (0.98, 0.003, 14.7) |
| the gather at 9 / 19 / 35 samples | 197 / 356 / about 640 µs | 7.9 / 14.7 / about 25.6 ms |

- **While building it:**
  - velocity and depth packed per pixel for the samples: gather −28 % (RTX) and −31 %
    (Radeon);
  - the tile pass's pixels interleaved across threads, so its writes coalesce: tiles
    95 → 72 µs and 2.23 → 0.98 ms (blocks of 5 × 5 per thread had made the packed writes
    206 µs and 8.2 ms).
- **Tests on both GPUs** (synchronization validation, no errors):
  - a still image comes through bit for bit;
  - a square moving 24 pixels a frame (a full shutter) streaks 12 pixels each way and no
    higher. Its core stays nearly opaque (9.5 of 10), so the energy grows by a fifth: the
    filter's "plausible" model;
  - a still object in front of a moving background stays within 0.002;
  - camera motion blurs along it only (0.01 across a row);
  - zero allocations in every antialiasing mode with motion blur;
  - every mode with it under validation;
  - the golden image `sponza_motion_blur` per vendor (a cinematic view, ADR 0067's
    requirement);
  - `ViewKind`'s unit tests.

## Consequences

- **Gameplay views pay nothing and cannot have it.** A cinematic view without motion blur
  pays nothing either.
- **Cutscenes at 1080p cost:**
  - 0.43 ms on the RTX;
  - 15.7 ms on the Radeon iGPU. Integrated GPUs should use fewer samples (9: 7.9 ms) or
    a lower output size for cutscenes.
  - Later: a half-size gather (Jimenez 2014) and 16-bit arithmetic, for the iGPU. The
    half-size gather is done (addendum below): 0.23 ms and 7.3 ms.
- **What the filter leaves:**
  - a moving object's core stays nearly opaque;
  - there is no blur across the edge of the screen;
  - translucent surfaces follow what is behind them (one velocity per pixel).
- **Object-only motion blur for gameplay views was ADR 0067's open question.** The owner's
  answer (2026-10-15): not wanted. Motion blur stays on cinematic views only.

## Addendum (2026-10-15): the gather at half size

The gather was the Radeon's cost: 14.7 of 15.7 ms. After Jimenez 2014 it now runs on a
texel per 2 × 2 pixels, and a composite at full size keeps what does not blur sharp.

- **Tiles.** The pass writes the velocity and depth of each 2 × 2's nearest pixel into one
  half-size texel, so a thinner foreground still stands in front.
  - r is rounded up to an even number of pixels (40 at 1080p, as before), so the 2 × 2s
    never straddle tiles.
- **Gather.** Each half-size texel samples as before from its 2 × 2's center (the color at
  full size, bilinear; the velocities and depths at half size). It keeps the samples apart
  from the center:
  - their color, premultiplied by their share of the result;
  - that share, Σw / (w_center + Σw).
- **Composite.** Each pixel of the image takes its own color, sharp, for the center's share
  and adds the blur of the four half-size texels around it.
  - The texels are bilinear, each weighted by 1 / (0.01 + |Δz| / z) against the pixel's
    own depth: a still edge in front keeps its own color.
  - Where nothing in a tile's neighbourhood moves, the pixel is copied unchanged.
- **Tests** (both GPUs, synchronization validation clean):
  - a still image still comes through bit for bit;
  - a square moving 24 pixels a frame streaks 12 pixels each way: 1.92 and 1.56 six
    pixels out, nothing 16 pixels out or above it, energy 1.21 times the square's;
  - a still object in front of a moving background stays within 0.002;
  - camera motion blurs along it only (0.005 across a row);
  - the golden image `sponza_motion_blur` was recorded again per vendor. It looks the
    same: mean FLIP 0.026 against the full-size gather, the grain of the samples' noise now
    per 2 × 2.
- **Measured** in the perf gate's `sponza_motion_blur` (1080p, 19 samples):

| | RTX 4060 Ti | Radeon iGPU |
|---|---:|---:|
| tiles | 72 → 35 µs | 0.98 → 1.20 ms |
| gather | 356 → 106 µs | 14.7 → 3.73 ms |
| composite | — → 85 µs | — → 2.35 ms |
| all four passes | 432 → 229 µs | 15.7 → 7.3 ms |
| GPU frame | 1,151 → 935 µs | 40.8 → 32.5 ms |

  - The Radeon's composite is bound by memory: it reads the image and the depth and
    writes the result, about 41 MB at 1080p. One gather for the four depths changed
    nothing.
