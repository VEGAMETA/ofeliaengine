# 0095. Depth of field: FidelityFX DOF's tiled gather, after the upscalers

- Status: Accepted
- Date: 2026-10-15
- Research: [docs/research/depth-of-field.md](../research/depth-of-field.md)
- Related:
  - [ADR 0072](0072-frame-renderer.md): the frame renderer;
  - [ADR 0075](0075-antialiasing-and-scaling-modes.md): the antialiasing and scaling modes;
  - [ADR 0060](0060-fsr-3-1-upscaling.md) and [ADR 0063](0063-dlss-through-streamline.md): the upscalers;
  - [ADR 0067](0067-no-camera-motion-blur-in-gameplay.md): only the camera's motion blur is restricted, not this;
  - queue item 6a.

## Context

- Focus draws the eye: aiming down sights, dialogue, cutscenes, photo modes.
- A view's frame: the scene, then antialiasing or an upscaler (TAAU, FSR, DLSS), then
  bloom and the tone mapper.
- What depth of field must get right:
  - a sharp object in front of a blurred background stays sharp;
  - a blurred object in front spreads over what is behind it;
  - a bright point out of focus becomes a disk;
  - a view without it pays nothing;
  - no patented method (the research note's check).

## Options

1. **Separable kernels** (Bevy's Gaussian and hexagons; McIntosh 2012, Garcia 2017).
   - Fast, with a constant cost per pixel.
   - But a blur crosses depth edges: the background bleeds over a sharp foreground, and
     a blurred foreground keeps a hard edge.
2. **One pass along a spiral** (Gustafsson 2018, Godot's circle).
   - Simple.
   - The cost grows with the blur's area, and the near field is weak.
3. **A tiled scatter-as-gather with near and far fields** (Jimenez 2014, Abadie 2018;
   FidelityFX DOF, Filament).
   - Bounded work per tile, and the near and far fields handled.
   - FidelityFX DOF 1.1's code is MIT.
4. **Sprites scattered per bright pixel** (Unreal's hybrid): expensive. Later, for
   highlights only.

Where it runs:

- **Before the upscalers, at render size** (Unreal). Cheaper on weak GPUs, but the
  upscalers then reproject a blurred foreground along the background's motion vectors (it
  ghosts), and Unreal adds a temporal pass of the depth of field's own for its noise.
- **After them, on the image bloom and the tone mapper read.** AMD's FSR guide places it
  there, and so do Filament and FidelityFX DOF.

## Decision

**Option 3, after the upscalers.**

`engine_render::dof::DepthOfField` has four compute passes, FidelityFX DOF 1.1's ported to
Slang (MIT, credited):

1. **Downsample.** Four bilateral mips of color and inverse depth, at half size and
   below. From the same reads, each 8 × 8 tile's nearest and farthest inverse depth and
   the widest reach of a tile (FidelityFX reads the depth again in a pass of its own).
2. **Dilate** the tiles by what reaches them.
3. **Blur** at half size:
   - tiles classified per wave (copy, near, far, both, constant radius);
   - rings outside-in;
   - the far field's rings occluding each other;
   - the near field weighted by 1 / r², with its opacity and a fill behind it.
4. **Composite** at full size: 3 × 3 medians, upscaling, a fixed 3 × 3 blur for small
   radii, the blend. It writes a new image.

Changes from FidelityFX:

- **Inverse depth instead of signed circles of confusion.** The tiles and mips hold
  inverse depth. A pixel's near and far radii are two separate non-negative values, so no
  value stands for both: US 8,629,868 claims such a single signed blur parameter.
- **The near field starts at a radius of one half-size pixel**, not at the plane in focus.
  - FidelityFX's way: a surface barely in front of focus joins the near field with a
    sharp pixel's weight. A blurred object in front of it then no longer spreads over it
    (Sponza's curtains).
  - The test: a background 0.8 pixels in front of focus. Before the change it cut a
    blurred object's spread, a quarter of its radius past its edge, from 28 % to 6 %.
- **No ring merging.**
  - FidelityFX reads the inner rings of flat tiles from coarser mips. A bright point's
    disk then becomes a cone, 1.7 times as bright in the middle, with a tail 8 pixels
    past its radius.
  - Merging would save 11 µs on the RTX and 0.27 ms on the Radeon (the blur of the strong
    Sponza scene below). We keep the disk; the owner (2026-10-15): merging as an option
    later.
- **Composite:**
  - the fixed 3 × 3 blur only where it shows: Radeon composite 3.38 → 3.22 ms;
  - shared memory as separate channels: RTX composite 128–138 → 115 µs.
- **The ring count is the same for a whole workgroup**, so the mip's texture is picked by
  a uniform index.
- **The output is a separate image**, so no workgroup reads pixels another writes: the
  output is deterministic.

**The lens** (`Lens`, `DofSettings`):

- The radius on the sensor is A · f / (S − f) · |S / D − 1|, with:
  - f the focal length, from the view's field of view and a sensor height (24 mm by
    default);
  - A the aperture's radius, f / (2 N) for an f-number N;
  - S the focus distance and D the pixel's distance.
- The settings: `focus_distance`, `f_number`, `sensor_height`, `max_radius` (4 % of the
  image's height) and `rings` (4).

**`engine_frame`:**

- `ViewSettings::depth_of_field` (`None` by default).
- It runs after antialiasing, upscaling and the previous color's copy (screen-space
  reflections keep a sharp history), before bloom.
- It reads the render-size depth.
- Its pipelines are made with the first view that uses it.

## Measurements

ref-1, release, 1920 × 1080. `scene_bench Sponza --sky --dof 3`: focused at 3 m, f/1.4 on
a 10 cm sensor, rings 4. A strong blur: the curtains in front and the atrium's far end.

| | RTX 4060 Ti | Radeon iGPU |
|---|---:|---:|
| f/1.4: downsample, dilate, blur, composite | 256 µs (56, 8, 76, 115) | 8.2 ms (1.86, 0.15, 2.98, 3.23) |
| f/5.6 | 201 µs | 6.4 ms |
| f/64 (almost nothing blurred) | 124 µs | 3.8 ms |
| rings 2 / 3 / 4 / 6, f/1.4 | 248 / 244 / 256 / 259 µs | 7.2 / 7.8 / 8.2 / 8.3 ms |
| GPU frame, `--post` → `--dof 3` | 745 → 950 µs | 22.7 → 30.2 ms |

- The perf gate's `sponza_dof` (no antialiasing, the same lens) matches the f/1.4 row.
- **Tests on both GPUs**, with no validation errors (synchronization validation on):
  - an image in focus comes through bit for bit;
  - a far point spreads to a disk of the lens's radius (13.6 pixels): flat inside, at most
    3.1 % of its mean 3 pixels past its edge, with 93 % of its energy;
  - a sharp object in front of a blurred background stays exact, and the background takes
    none of its color;
  - a blurred object spreads over a sharp background, a blurred one, and one barely in
    front of focus, and leaves the background exact beyond its radius;
  - no allocation per frame in every antialiasing mode;
  - every mode under validation;
  - the golden image `sponza_dof` per vendor.

## Consequences

- A view without depth of field pays nothing.
- **On the RTX: 0.12–0.26 ms at 1080p.**
- **On the Radeon iGPU: 3.8–8.2 ms.**
  - The composite and the downsample are bound by memory: about 60 and 30 MB a frame at
    1080p.
  - The blur is bound by arithmetic.
  - The cost follows the output's size, not the render size.
  - Later, for the iGPU:
    - the blur in 16-bit arithmetic (FidelityFX's half-precision path; packed on RDNA);
    - the composite in place for blurred tiles only, or folded into the tone mapper;
    - a quarter-size gather.
- **With temporal upscalers** the depth is at render size. The edge between a sharp and
  a blurred surface steps by a render pixel.
- **With SMAA S2x** the samples' difference is added after depth of field. It mostly
  cancels between the two images.
- **Later:**
  - bokeh shapes (blades);
  - focus measured on the GPU (autofocus);
  - sprites for bright highlights;
  - translucent surfaces (one depth per pixel, as in FidelityFX).

## Addendum (2026-10-15): the integrated GPU's costs, measured pass by pass

Queue item 6b. Sponza at 1080p (`scene_bench --sky --dof 3`), on the Radeon iGPU unless
said otherwise. Before: f/1.4 8.23 ms (downsample 1.86, dilate 0.15, blur 2.98, composite
3.25); f/64 3.84 ms (blur 0.74, composite 1.15).

What bounds each pass, from variants of the shaders:

- **Composite: the latency of its reads.**
  - Without the per-pixel depth reads it took 2.52 ms instead of 3.25. They came after its
    waits for shared memory, where nothing hid them.
  - Without the full-size color: −7 %. Without the medians: −3 %.
- **Blur: arithmetic.**
  - Without any ring sample 0.62 ms; without the samples' texture reads (the same
    arithmetic) 2.51 ms. Its 61 samples per pixel cost 2.36 ms, a sixth of it fetching.
- **Downsample: memory.**
  - Without the depth reads 1.38 ms (−26 %).
  - The four depths of a quad in one gather changed nothing.
  - Its time moves with where its images land in the iGPU's memory (1.15–1.87 ms across
    the bench's two scenes, same sizes).

Done, the same image bit for bit (`sponza_dof`: FLIP 0 on both GPUs):

1. **The composite reads its depths first,** and loads every texel of its windows before
   it stores any to shared memory.
   - Composite at f/1.4: Radeon 3.25 → 2.82 ms; RTX 124 → 100–109 µs.
2. **The blur's waves in focus write their centers only where the composite reads them.**
   - The texels of a blur group are read only by the composite's group over the same
     pixels and its 8 neighbors (their 11 × 11 windows); where none blends the fields,
     nothing is written.
   - Blur at f/64: Radeon 740 → 420 µs; at f/1.4 unchanged.
   - Checking every group before its gather instead: 127 µs at f/64, but +4 % at f/1.4
     (the check's 36 tile reads before the gather). Not taken.

Totals: **f/1.4 8.23 → 7.82 ms, f/64 3.84 → 3.52 ms; RTX f/1.4 276 → 242–263 µs.**

Not done, and why:

- **The composite in place** (sharp groups would write nothing: f/64's composite
  1.15 → 0.16 ms).
  - With TAAU, the default on every device class, the image depth of field blurs is TAAU's
    history for the next frame: it cannot be written in place.
  - A copy for the temporal modes would cost what it saves.
- **16-bit arithmetic in the blur.**
  - Its accumulators can overflow half precision (inputs up to 65,000, about 60 samples).
  - They would stay 32-bit, so only part of the arithmetic would pack.
  - Not tried for that reason.
- **Fewer rings** stay the game's choice (`DofSettings::rings`): 2 rings cost the Radeon
  6.85 ms at f/1.4 (blur 2.06 ms), FLIP 0.036 (mean) against 4. The strongest blur comes
  from coarser levels, a little smoother.
- **A quarter-size gather** (estimated: blur and fields a quarter, about −2.5 ms at f/1.4
  on the Radeon) blurs regions near focus from quarter-size fields. Left until a game
  needs it on an integrated GPU.
