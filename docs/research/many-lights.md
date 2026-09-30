# Many lights: tiled, clustered and z-binned light culling

Checked 2026-09-30. Sources at the end. No code was copied.

## Why

The forward renderer shades one directional light. Interiors, streets at night, muzzle
flashes and effects need hundreds to thousands of local lights (point and spot).
Evaluating every light for every pixel costs lights × pixels. Culling must find, per
pixel, the few lights that reach it, cheaply and without losing any.

## How others do it

- **Tiled forward, "Forward+"** (Harada, McKee, Yang, Eurographics 2012):
  - a compute pass lists the lights per screen tile (16 × 16), culled against the tile's
    frustum and its depth bounds from a depth prepass;
  - shading reads the tile's list.
  - Depth discontinuities in a tile (a character in front of a wall) make the list long:
    the tile's depth range spans both.
- **Clustered shading** (Olsson, Billeter, Assarsson, HPG 2012; Persson, "Practical
  Clustered Shading", SIGGRAPH 2015):
  - the frustum is split into 3D cells ("froxels"): screen tiles × depth slices spaced
    exponentially;
  - a list of lights per cell;
  - no depth prepass needed, and it works for transparent surfaces and volumetrics too.
  - Memory grows with cells × lights per cell: lists need a maximum or a global index
    buffer with offsets.
- **Doom (2016)** (Sousa and Geffroy, SIGGRAPH 2016):
  - clustered forward with 16 × 8 × 24 = 3,072 clusters, logarithmic depth slices;
  - up to 256 lights, 256 decals and 256 probes per cluster;
  - lists built on the CPU.
- **Bevy** (MIT/Apache, `bevy_light::cluster`, main):
  - 4,096 clusters with 24 exponential depth slices (the first ends at 5 m), following
    Doom;
  - assignment on the CPU, or on the GPU when available.
- **Z-binning** (Drobot, "Improved Culling for Tiled and Clustered Rendering", Call of
  Duty: Infinite Warfare, SIGGRAPH 2017):
  - visibility is split into two independent parts intersected per pixel:
    - lights sorted by view depth; for each **z-bin** (a slice of view depth) the
      minimum and maximum index of the lights that overlap it;
    - for each **screen tile**, a **bitmask** with one bit per light (sorted order)
      whose screen bounds cover the tile;
  - a pixel reads its tile's words between the bin's minimum and maximum index, masks
    the bits outside that range, and iterates the remaining bits;
  - the loop is scalarized: the wave iterates the OR of its lanes' masks, so it walks
    one light at a time together.
  - Memory is tiles × lights / 8 bytes plus the bins, with no per-cell maximum and no
    lists to compact. Depth discontinuities in a tile cost nothing: depth is resolved
    per pixel by the bins.
  - Sebastian Sylvan's analysis ("Thoughts on light culling for clustered shading")
    reads it as O(X·Y·N + Z + N) memory against O(X·Y·Z·N) for full clusters.
- **Godot 4** (MIT): the "Forward+" renderer builds clusters by rasterizing light
  volumes into a cluster buffer.

## Conclusions

1. **Z-binning with tile bitmasks:**
   - no per-cell limits, which suits "thousands of lights";
   - linear memory;
   - depth resolved per pixel, which suits our forward renderer (no depth prepass by
     default, ADR 0042);
   - GPU-friendly: bit operations, wave-uniform loops.
2. **The CPU:**
   - culls lights against the view frustum;
   - sorts them by view depth (radix sort of the depth keys, like the 2D renderer's);
   - fills the z-bins;
   - computes each light's conservative screen rectangle in tiles.
3. **One compute pass** builds the tile bitmasks: one thread per (tile, 32-light word),
   testing each light's rectangle.
4. **Shading:**
   - the bin from the pixel's view depth, the words in its range, masked, scalarized;
   - point and spot lights with inverse-square falloff windowed to zero at the light's
     range (Karis 2013, as in Filament and Unreal).
5. **A brute-force mode** (every light for every pixel) as the reference for tests.
6. **Later:**
   - tighter sphere bounds (Mara and McGuire 2013);
   - conservative proxy rasterization (Drobot);
   - shadows for local lights;
   - the same bins for decals and reflection probes.

## Sources

- Harada, McKee, Yang, "Forward+: Bringing Deferred Lighting to the Next Level", Eurographics 2012 short paper
- Olsson, Billeter, Assarsson, "Clustered Deferred and Forward Shading", HPG 2012
- Persson, "Practical Clustered Shading", SIGGRAPH 2015 course
- Sousa, Geffroy, "The devil is in the details: idTech 666", SIGGRAPH 2016 Advances (cluster numbers via Courrèges, "DOOM (2016) Graphics Study": https://www.adriancourreges.com/blog/2016/09/09/doom-2016-graphics-study/)
- Drobot, "Improved Culling for Tiled and Clustered Rendering", SIGGRAPH 2017 Advances (https://advances.realtimerendering.com/s2017/)
- Sylvan, "Thoughts on light culling for clustered shading" (https://www.sebastiansylvan.com/post/light_culling/)
- Bevy, `crates/bevy_light/src/cluster/mod.rs`, main, checked 2026-09-30 (MIT OR Apache-2.0)
- Karis, "Real Shading in Unreal Engine 4", SIGGRAPH 2013 (windowed inverse-square falloff)
- Mara, McGuire, "2D Polyhedral Bounds of a Clipped, Perspective-Projected 3D Sphere", JCGT 2013
