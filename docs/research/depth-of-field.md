# Depth of field

Checked 2026-10-15. Sources at the end.

- Code read: Bevy (MIT or Apache-2.0), Godot (MIT), Filament (Apache-2.0), AMD FidelityFX DOF
  1.1 (MIT).
- Unreal's DOF is known only from its talk and documentation.
- No code was copied. What we take from FidelityFX DOF is ported and credited in
  `THIRD_PARTY_NOTICES.md`.

## Why

Focus draws the eye: aiming down sights, dialogue, cutscenes, photo modes, tilt-shift
looks. Queue item 6a. Only the camera's own motion blur is restricted by the team
(ADR 0067); depth of field is allowed in gameplay.

## How others do it

### Bevy

`bevy_post_process::dof`, main.

- The circle of confusion (CoC) of each pixel comes from a physical camera: the focal
  length from the field of view and a sensor height (Super 35 by default), and an f-number.
- Two modes:
  - a separable Gaussian, in two passes;
  - a hexagonal bokeh, made of three skewed box blurs in two passes (Barré-Brisebois 2017).
- Near and far are not separated:
  - the far field's blur bleeds over sharp objects in front of it;
  - a blurred foreground keeps a hard silhouette, because it does not spread over what
    lies behind it.

### Godot

`renderer_rd/effects/bokeh_dof.cpp`, `shaders/effects/bokeh_dof.glsl`, master.

- A blur size per pixel, from artist near and far ranges or a physical curve.
- Three kernels:
  - box and hexagon: separable directional passes, a sample counted when its size covers
    the distance;
  - circle: one pass along a golden-angle spiral (after Gustafsson 2018). A sample behind
    the center is clamped to twice the center's size, so the background does not bleed
    over a sharp foreground.
- Full or half resolution, optional jitter, composited by blur size.

### Filament

`filament/src/materials/dof/*`, `PostProcessManager::dof`, main.

**Placement.** After TAA, also with TAA upscaling, reading the render-size depth.
`Renderer.cpp` notes that DOF belongs before TAA, but there the instability of highlights
makes fireflies, unless DOF gets a temporal pass of its own (Abadie).

**Its passes, at half resolution:**

1. **CoC and downsample.** The CoC as a multiply-add of the depth value, from the physical
   camera (aperture, focal length, focus distance, a 24 mm sensor). The 2 × 2 colors go
   down with a firefly weight, the CoC as the minimum of the four (the foreground wins).
2. **Mips.** A 4-level mip chain.
3. **Tiles.** Min and max CoC per 16-pixel tile, dilated by one tile.
4. **Gather in rings**, per tile type (in focus: skipped; constant CoC: plain average;
   foreground; background), with noise against banding:
   - the background uses Abadie's ring buckets: rings outside-in, each ring occluding
     the ones before by their average CoC;
   - the foreground uses Jimenez's two layers;
   - "mirror" hole filling;
   - more blur reads coarser mips.
5. **Median** of 3 × 3.
6. **Combine** at full resolution by alpha.

### FidelityFX DOF 1.1 (AMD)

`sdk/include/FidelityFX/gpu/dof/*`, `sdk/src/components/dof/ffx_dof.cpp`, SDK v1.1.4.
Five compute passes; the work runs at half resolution.

1. **Depth tiles.** The min and max depth of each 8 × 8 tile, turned into signed CoC radii,
   plus a global maximum tile radius by atomic max.
2. **Color mips.** Four bilateral mip levels of color and CoC (built with SPD). A 2 × 2
   quad's pixels are weighted by how far their CoC is from the first pixel's (none beyond
   one pixel).
3. **Dilation.** The tiles dilated by scatter-as-gather over the global maximum: a tile
   spreads its near and far radii to the tiles they reach.
4. **Blur:**
   - **Classification per wave, from the dilated tiles:**
     - in focus: copied;
     - near only, far only, or both;
     - nearly constant CoC: "color only", equal weights.
   - **Rings outside-in:** about 6.75 samples per ring index. The ring count is the
     kernel radius in pixels, capped by the quality setting. A higher mip covers the
     rings left out, and inner rings merge in flat tiles.
   - **Far field:** two buckets per ring (this ring, the rings before). The current bucket
     merges into the previous one with an occlusion estimate from the rings' radii and
     coverages (Abadie).
   - **Near field:** each sample counts with weight 1 / r² when its radius reaches the
     center. Its opacity comes from the weight gathered. A "fill" of background samples
     goes behind it.
5. **Composite at full resolution:**
   - a 3 × 3 median of luminance for both fields, the near field's opacity averaged;
   - bilinear upscaling, invalid near texels rejected;
   - a fixed 3 × 3 blur for CoC between 0.5 and 2 pixels;
   - blending by the pixel's CoC; the near field last.

Its guidance: place it after TAA, since the blur amplifies instability. It keeps one
depth per pixel (no translucency) and a circular kernel (other shapes through callbacks).

### Unreal Engine: Diaphragm DOF

Abadie, "A Life of a Bokeh", SIGGRAPH 2018; Unreal's documentation. The talk describes:

- a thin-lens CoC;
- a half-resolution gather with tile classification, ring buckets for the background,
  and mirrored hole filling for the foreground;
- a "slight out of focus" convolution at full resolution;
- hybrid scatter: sprites for bright, isolated pixels;
- a temporal pass of its own, since it runs before TAA.

Unreal's code is not open source. Ideas only.

### Jimenez, CoD: Advanced Warfare (SIGGRAPH 2014)

The basis of the tiled gathers above:

- scatter-as-gather at half resolution;
- a presort into foreground and background;
- a 49-tap ring kernel whose samples count by their own CoC (weight 1 / (π r²));
- tile max CoC with neighbourhood dilation, so foregrounds spread;
- a 3 × 3 post-filter to close the kernel's gaps;
- kernel rotation, left to TAA to average.

### Separable kernels

McIntosh et al. 2012 (polygonal apertures), Barré-Brisebois 2017 (hexagons), Garcia 2017
(circles as sums of complex Gaussian phasors, Frostbite):

- fast, with large radii at a constant cost per pixel;
- no near / far occlusion: a blur crosses depth discontinuities;
- Garcia's circles need 2–3 complex components per channel.

## What matters for us

### Placement

- **After antialiasing and upscaling** (TAAU, FSR, DLSS), on the image bloom and the tone
  mapper read:
  - AMD's FSR integration guide lists depth of field among the effects that run after
    the upscaler, at presentation resolution;
  - Filament and FidelityFX DOF place it after TAA.
- **Why not before.** The upscalers then reproject a sharp image along its own motion
  vectors. A foreground blurred before them would spread over pixels whose motion
  vectors belong to the background, and it would ghost.
- **What it costs us.** No temporal accumulation follows to average a noisy kernel. The
  sampling must be stable from frame to frame: no per-frame rotation, a fixed hash at
  most.
- **Depth stays at render size**, as with Filament's TAA upscaling. Each DOF pixel reads
  the depth texel under it.

### Patents

Checked 2026-10-15 by targeted searches; Google Patents' legal status is its own estimate.

- **US 8,629,868 B1**, Rockwell Collins (filed 2010, active until 2032-03-08).
  - Claims 1 and 5: "a single blur parameter for each of the plurality of objects that
    represents both a magnitude of blur and a relative distance". It has a base value in
    focus, is higher for the far field and lower for the near field, or the reverse.
  - Claim 7: anti-aliasing that picks such blur values without averaging.
  - A signed CoC, as FidelityFX and Filament store it, is that parameter.
  - **We never form one.** The tiles and mips hold inverse depth. The near and far radii
    are two separate non-negative quantities computed from it, each zero outside its own
    field.
- **US 9,305,375 B2**, Lytro, now Google (active until 2034-08-12).
  - It blurs "an image projected from light field data" by circles of confusion at two
    depths for each subject pixel. Pixels of the farther set that the nearer set occludes
    are excluded.
  - Its claims are limited to light-field images. Our gather tests each sample's own
    radius against its distance, not circles per depth plane.
- **US 11,120,528 B1**, Apple (active until 2039-11-18).
  - It modifies default "shallow depth of field" parameters by an adjustment factor
    computed from a designated artificial aperture and a tuning aperture.
  - Our radii come from the thin-lens formula with the f-number. No parameter is scaled
    from a tuning aperture.
- **US 10,726,524 B2**, Qualcomm (tile classification and low-resolution blur of bokeh on
  mobile devices): expired for unpaid fees.
- **US 7,787,688 B1**, Pixar (DOF by simulated heat diffusion): not our method.

### Costs

- The work runs at half resolution.
- **Bounded per tile:**
  - in-focus tiles are skipped;
  - the ring count is capped;
  - larger radii read coarser mips instead of more rings.
- **Samples per pixel:** about 6.25 · n(n + 1) / 2 for n rings: 63 for 4, 131 for 6.
- **Weak GPUs.** At 1080p, the Radeon iGPU fetches about 16 G texels per second. Four
  rings over a half-resolution image cost it about 2 ms in texture fetches alone, so its
  default must be lower.

## Conclusions

1. **FidelityFX DOF 1.1's passes, ported to Slang** (MIT, credited), with these changes:
   - inverse depth where it keeps signed CoC (the depth tiles, the mips), and the near
     and far radii as two separate non-negative values (US 8,629,868);
   - the CoC from the thin lens: the view camera's field of view, a sensor height, an
     f-number and a focus distance;
   - placed after antialiasing and upscaling, reading the render-size depth;
   - the composite writes a separate image, so no workgroup reads pixels another one
     writes (deterministic output, tested against references);
   - fixed sampling positions: no temporal accumulation follows.
2. **Settings per view** in `engine_frame`, off by default. Costs nothing when off: no
   pass, no pipeline made.
3. **Tests:**
   - the CPU thin-lens radii;
   - an in-focus image comes through bit for bit;
   - a far point spreads to a disk of the expected radius, keeping its energy;
   - a sharp object in front of a blurred background stays sharp;
   - a blurred foreground spreads over a sharp background;
   - golden images, the perf gate and zero allocations.
4. **Later:** bokeh shapes (blades), GPU autofocus, bright highlights scattered as
   sprites, translucent surfaces.

## Measured while building it (2026-10-15, ref-1)

- **FidelityFX's ring merging** reads a flat tile's inner rings from coarser mips.
  - It turns a bright point into a cone: 1.7 times as bright at its center as the disk's
    mean, with a tail 8 pixels past its radius.
  - Without it the disk is flat to within 16 % of its mean, its edge soft over 3 pixels.
  - Merging saves the strong Sponza scene's blur 11 µs on the RTX and 0.27 ms on the
    Radeon. We keep the disk.
- **FidelityFX's near field starts at the plane in focus.**
  - A surface barely in front of focus (0.8 half-size pixels of radius) then joins the
    near field with a sharp pixel's weight (1 / r² = 1).
  - A blurred object in front of it spreads 6 % of its color a quarter of its radius past
    its edge, instead of 28 %. Sponza's curtains kept hard silhouettes.
  - From a radius of one pixel, the composite's fixed blur covers what is below.
- **FidelityFX reads the depth twice:** once for its tiles, once for its mips.
  - From one read, the tiles cost nothing extra: RTX −14 µs, Radeon −0.38 ms.
- **Composite:**
  - Shared memory as separate channels (FidelityFX's own 32-bit layout): RTX 128–138 →
    115 µs.
  - The fixed 3 × 3 blur only where it shows: Radeon 3.38 → 3.22 ms.
- **Costs** at 1080p (Sponza, focused at 3 m, f/1.4 on a 10 cm sensor, 4 rings):
  - RTX 4060 Ti: 256 µs.
  - Radeon iGPU: 8.2 ms. Its composite and downsample are bound by memory (about 60 and
    30 MB a frame), its blur by arithmetic.
  - With almost nothing blurred (f/64): 124 µs and 3.8 ms.

## Sources

- Bevy, `crates/bevy_post_process/src/dof/mod.rs`, `dof.wesl`, main, checked 2026-10-15 (MIT OR Apache-2.0)
- Godot, `servers/rendering/renderer_rd/effects/bokeh_dof.cpp`, `shaders/effects/bokeh_dof.glsl`, master, checked 2026-10-15 (MIT)
- Filament, `filament/src/materials/dof/*.mat`, `dofUtils.fs`, `filament/src/PostProcessManager.cpp` (`dof`), `filament/src/details/Renderer.cpp`, main, checked 2026-10-15 (Apache-2.0)
- AMD FidelityFX DOF 1.1, FidelityFX SDK v1.1.4: `sdk/include/FidelityFX/gpu/dof/*.h`, `sdk/src/components/dof/ffx_dof.cpp`, `sdk/include/FidelityFX/host/ffx_dof.h`; documentation at gpuopen.com/manuals/fidelityfx_sdk/techniques/depth-of-field, checked 2026-10-15 (MIT)
- AMD FidelityFX Super Resolution 3 integration guide, "Placement in the frame", gpuopen.com, checked 2026-10-15
- J. Jimenez, "Next Generation Post Processing in Call of Duty: Advanced Warfare", SIGGRAPH 2014 Advances in Real-Time Rendering
- G. Abadie, "A Life of a Bokeh", SIGGRAPH 2018 Advances in Real-Time Rendering
- T. Sousa, "Graphics Gems from CryENGINE 3", SIGGRAPH 2013
- E. Hammon, "Practical Post-Process Depth of Field", GPU Gems 3, ch. 28, 2007
- J. Demers, "Depth of Field: A Survey of Techniques", GPU Gems, ch. 23, 2004
- L. McIntosh, B. Riecke, S. DiPaola, "Efficiently Simulating the Bokeh of Polygonal Apertures in a Post-Process Depth of Field Shader", Computer Graphics Forum 31 (6), 2012
- C. Barré-Brisebois, "Hexagonal Bokeh Blur Revisited", 2017
- K. Garcia, "Circular Separable Convolution Depth of Field", SIGGRAPH 2017 Talks
- D. Gustafsson, "Bokeh depth of field in a single pass", blog.voxagon.se, 2018
- M. Potmesil, I. Chakravarty, "A Lens and Aperture Camera Model for Synthetic Image Generation", SIGGRAPH 1981 (the thin-lens CoC)
- Patents: US 8,629,868 B1, US 9,305,375 B2, US 11,120,528 B1, US 10,726,524 B2, US 7,787,688 B1 (Google Patents, checked 2026-10-15)
