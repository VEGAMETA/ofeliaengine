# Motion blur

Checked 2026-10-15. Sources at the end.

- Code read: Bevy (MIT or Apache-2.0).
- Read as papers or documentation: McGuire et al. 2012, Guertin et al. 2014, Jimenez 2014,
  NVIDIA's GameWorks samples, Unreal (documentation).
- Blender EEVEE (GPL): its commit notes only, no code.
- No code was copied.

## Why, and the team's limit

A shutter open for part of a frame records what moves as a streak. In cutscenes it reads as
film, and fast motion stops strobing. ADR 0067 (team decision, 2026-10-01) limits its use:

- **Blur from the camera's own motion is allowed only on cinematic views.** These are
  non-interactive: cutscenes and sequences.
- **A gameplay view has none to turn on.**
- **Object motion blur in gameplay is a separate question**, still open.

The full effect (camera and object motion) is built and tested; only its use is limited.

## How others do it

### Bevy

`bevy_post_process::motion_blur`, main.

- One full-screen pass:
  - each pixel gathers `2 · samples` taps along its own motion vector, scaled by a shutter
    angle (0.5 by default), jittered by interleaved gradient noise;
  - a tap counts only if it lies behind the pixel, or if its own motion, projected on
    the pixel's, reaches the pixel.
- Cheap and robust for the camera's own motion.
- Bevy documents its limit: a fast object in front of a still background does not blur
  past its own silhouette. Its pixels gather; nothing gathers it into its neighbours.

### McGuire, Hennessy, Bukowski, Osman: "A Reconstruction Filter for Plausible Motion Blur" (I3D 2012)

The scatter-as-gather filter that most engines use since:

1. **TileMax:** the largest velocity of each K × K tile. Velocities are clamped to K,
   and K is the largest blur.
2. **NeighborMax:** the largest of the 3 × 3 tiles around, so a pixel knows the farthest
   velocity that can reach it.
3. **The gather.** Each pixel takes N samples along the dominant velocity, jittered.
   - Three cases weigh each sample:
     - a farther object blurring over the pixel: a cone of the sample's velocity;
     - the pixel's own blur revealing what is behind it: a cone of the pixel's velocity;
     - both together: two cylinders.
   - A soft depth comparison tells foreground from background.
   - The center carries a weight of its own.
- **Artifacts:** one direction per tile.
  - Where motions overlap (a spinning wheel on a moving car), the tile's direction wins.
  - At tile edges the direction jumps.
  - Thin objects ghost.

### Guertin, McGuire, Nowrouzezahrai: "A Fast and Stable Feature-Aware Motion Blur Filter" (HPG 2014)

McGuire 2012 made robust (the paper's Appendix A gives the pseudocode):

- **Two directions.** Samples alternate between the neighbourhood's dominant velocity and
  the pixel's own.
  - A variance metric could split them better. The authors saw no difference in motion
    and disabled it.
- **Feature-aware weights:** each case's weight is multiplied by the dot product of the
  directions involved.
  - The cone of a sample blurring over the pixel: the sample's direction with the sampling
    direction.
  - The cone of the pixel's own blur: the pixel's direction with it.
  - The two cylinders: the larger of the two.
- **Tile edges.** The NeighborMax lookup is jittered stochastically near tile borders,
  with a probability falling off linearly over the tile (slope η). It never reaches into a
  diagonal tile. A diagonal tile counts in NeighborMax only if its velocity points at the
  center tile.
- **The center weight:** N / (κ ‖v_c‖), normalized against N. The sample nearest the
  center is kept like the others.
- **A relative depth comparison:** `zCompare(a, b) = clamp(1 − (a − b) / min(a, b), 0, 1)`,
  independent of the scene's scale.
- **Velocities** clamped to [0.5, r] pixels: under half a pixel, nothing blurs.
- **Sample positions:** uniform along the line, jittered per pixel by a Halton value.
  - The jitter spans τ pixels, scaled by ψ / N, so it shrinks as N grows.
- **Parameters for every scene:** {N, r, η, κ, τ, γ, ψ} = {35, 40, 1, 40, 0.95, 1.5, 27}.
  - γ eases the center's direction from perpendicular to its own over 1.5 pixels of speed.
- **Cost:** N = 35 at 1080p, 3.8 ms on a GTX 780; N = 19, 2.0 ms. The noise is high
  frequency, which a post-process antialiasing pass after it smooths.

### Jimenez, CoD: Advanced Warfare (SIGGRAPH 2014)

McGuire's filter at half resolution:

- mirrored samples recover the background behind a moving foreground;
- the TileMax and NeighborMax passes are shared with depth of field;
- noise is averaged by TAA afterwards.

### Unreal Engine

Documentation only. Unreal flattens the velocity and builds tile max and min velocities,
spreading them by drawing quads per tile along their velocity. It gathers in each tile's
direction, before TAA. Its look is McGuire's family.

### NVIDIA GameWorks samples (D3D, GL4)

TileMax, NeighborMax and the gather, after McGuire 2012, as a reference implementation.

## What matters for us

- **Where.** After antialiasing and the upscalers, after depth of field, before bloom and
  the tone mapper.
  - The same place as depth of field (ADR 0095), for the same reasons: the upscalers
    reproject a sharp image, and AMD's FSR guide lists motion blur among the effects after
    them.
  - No TAA follows to average noise. Guertin's jitter (ψ / N) keeps it fine-grained.
    Post-tone-mapping filters (FXAA, SMAA) smooth it when chosen.
- **Inputs we have.**
  - Motion vectors: render size, RG16F, texture-coordinate delta current − previous,
    without jitter. Background pixels carry the camera's motion.
  - Linear depth from the depth buffer.
  - A cinematic view asks for motion vectors like a temporal one.
- **Exposure.** The blur's length is the shutter fraction times this frame's motion. A
  fixed shutter angle at a higher frame rate blurs less per frame, as film does.
- **Cuts.** After a cut, the motion vectors of the first frame are zero: nothing blurs.
- **Patents** (checked 2026-10-15 by targeted searches):
  - Found and not ours: stochastic rasterization of motion and defocus blur (Intel: US
    2012/0293515, US 2015/0022532); layered light-field reconstruction (US 9,483,869, US
    9,672,657); velocity-adaptive sampling rates (US 2018/0197304, US 10,930,022).
  - None found on the tile-max / neighbour-max reconstruction filter. McGuire's and
    Guertin's papers are public since 2012 and 2014, and engines ship the method
    (Sousa 2013, Jimenez 2014).

## Conclusions

1. **Guertin et al. 2014's filter, written from the paper:**
   - TileMax and NeighborMax (diagonals only if they point at the tile), and the jittered
     NeighborMax lookup;
   - two directions, alternating;
   - feature-aware weights;
   - the relative depth comparison;
   - the normalized center weight;
   - jitter scaled by ψ / N, from interleaved gradient noise in place of Halton values
     (fixed per pixel).

   One more thing was learned while building it (ADR 0096). The weights' distance is the
   paper's T = t ‖v_max‖ for both kinds of samples. With each sample's own distance, the
   samples along a still pixel's zero velocity all land on it with a sharp sample's
   weight, and the streak past a moving object fades: 6 pixels out, 0.24 instead of 1.73.
2. **Settings:**
   - the shutter (fraction of the frame, 0.5 = 180°);
   - the samples (19 by default, the paper's cheaper row; 35 its quality);
   - the largest blur (the tile size: 40 pixels at 1080p, as the paper).
3. **API (ADR 0067):**
   - `ViewSettings::kind`: `ViewKind::Gameplay` (default) or
     `ViewKind::Cinematic { motion_blur }`;
   - the settings live inside the cinematic variant, so a gameplay view cannot express
     them;
   - `engine_runtime`'s cinematic cameras get a shutter.
4. **Tests:**
   - a still scene comes through unchanged;
   - a moving bright square blurs along its motion, by the shutter's length, keeping its
     energy;
   - a still object in front of a moving background stays sharp;
   - camera motion blurs everything in a cinematic view;
   - a gameplay view cannot ask for any of it (the type);
   - golden image of a cinematic view, zero allocations.
5. **Later, if the team wants it:** object-only motion blur for gameplay views, with the
   camera's motion subtracted from the motion vectors (ADR 0067's open question).

## Sources

- M. McGuire, P. Hennessy, M. Bukowski, B. Osman, "A Reconstruction Filter for Plausible Motion Blur", I3D 2012
- J.-P. Guertin, M. McGuire, D. Nowrouzezahrai, "A Fast and Stable Feature-Aware Motion Blur Filter", HPG 2014 (Appendix A: pseudocode; Section 5: parameters)
- J. Jimenez, "Next Generation Post Processing in Call of Duty: Advanced Warfare", SIGGRAPH 2014 Advances in Real-Time Rendering
- T. Sousa, "Graphics Gems from CryENGINE 3", SIGGRAPH 2013
- Bevy, `crates/bevy_post_process/src/motion_blur/mod.rs`, `motion_blur.wesl`, main, checked 2026-10-15 (MIT OR Apache-2.0)
- NVIDIA GameWorks, "Motion Blur Advanced Sample" (D3D11, GL4), documentation, checked 2026-10-15
- Unreal Engine documentation, "Motion Blur" (post-process settings), 5.x, checked 2026-10-15
- Blender EEVEE motion blur commits (bf-blender-cvs, 2020): read for the velocity tile pre-pass idea only (GPL, no code)
- AMD FidelityFX Super Resolution 3 integration guide, "Placement in the frame", checked 2026-10-15
- Patents: US 2012/0293515, US 2015/0022532, US 9,483,869, US 9,672,657, US 2018/0197304, US 10,930,022 (Google Patents and USPTO, checked 2026-10-15)
