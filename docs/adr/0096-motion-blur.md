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
  - Later: a half-size gather (Jimenez 2014) and 16-bit arithmetic, for the iGPU.
- **What the filter leaves:**
  - a moving object's core stays nearly opaque;
  - there is no blur across the edge of the screen;
  - translucent surfaces follow what is behind them (one velocity per pixel).
- **Object-only motion blur for gameplay views is ADR 0067's open question.** If the team
  wants it: the same passes with the camera's motion subtracted from the motion vectors.
