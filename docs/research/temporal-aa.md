# Temporal antialiasing and upsampling (TAA / TAAU), motion vectors

Checked 2026-09-27. Sources at the end. No code was copied; formulas are from the papers
and talks, and Bevy's shader (MIT OR Apache-2.0) was read for its structure.

## Why

- **Aliasing is what remains ugly.** After materials, the worst artifacts are aliasing:
  - geometric edges, and alpha-tested foliage in particular;
  - specular aliasing that geometric specular AA only partly removes;
  - shadow-map aliasing.
- **MSAA does not help much here.** It fixes only geometric edges and is expensive with
  a forward renderer's per-sample storage.
- **Temporal accumulation handles all of them** by jittering the projection by a
  sub-pixel offset every frame and blending each pixel with its reprojected history.
- **TAAU,** the same accumulation into a larger output, is the engine's own upscaler
  (CLAUDE.md §6). It is the fallback when FSR or DLSS are absent, and the quality
  reference.

## How others do it

- **Karis, "High Quality Temporal Supersampling"** (SIGGRAPH 2014, Unreal Engine 4):
  - jitter from a Halton sequence;
  - motion vectors from the previous transforms;
  - the history sample is clamped to the current 3 × 3 neighbourhood in YCoCg;
  - velocity is taken from the nearest depth in the neighbourhood (moving edges keep
    their motion);
  - luminance weighting (1 / (1 + luma)) against flicker from HDR outliers;
  - the current frame is reconstructed with a Blackman-Harris kernel
    (≈ exp(−2.29 x²)) around the jittered sample positions.
- **Salvi, "An Excursion in Temporal Supersampling"** (GDC 2016): **variance clipping**.
  The box is mean ± γσ of the neighbourhood (γ ≈ 1), and history is clipped towards
  its centre instead of clamped per channel. Less ghosting and less flicker than a
  min/max box.
- **Pedersen, "Temporal Reprojection Anti-Aliasing in INSIDE"** (GDC 2016, Playdead,
  MIT code): clipping towards the box centre in YCoCg, closest-depth velocity.
- **Jimenez et al. / Activision, "Dynamic Temporal Antialiasing and Upsampling in Call of
  Duty"** (2017–2020): a 5-tap Catmull-Rom history fetch (the four corner taps dropped),
  upsampling from jittered render-resolution samples.
- **AMD FidelityFX Super Resolution 2** (MIT, documented stages):
  - reconstruct and dilate (nearest-depth motion vectors, previous depth reconstructed
    by atomics);
  - depth clip (disocclusion);
  - locks for thin features;
  - reproject and accumulate (current samples upsampled with Lanczos);
  - RCAS sharpening.

  Inputs are jittered color and depth, and unjittered motion vectors. The jitter sequence
  length is ⌈8 · scale²⌉ Halton(2, 3) phases, and the application resets history on
  camera cuts.
- **Bevy (MIT OR Apache-2.0, `bevy_anti_alias::taa`):**
  - Karis' structure: nearest-depth velocity, 5-tap Catmull-Rom history, 3 × 3
    variance clipping in YCoCg on reversibly tonemapped colors (x / (1 + max));
  - a per-pixel confidence in history alpha, which lets static pixels accumulate up to
    1/0.015 frames, with a blend of 0.1 when moving;
  - history reset off screen. No upscaling.
- **Godot** uses FSR 2 for upscaling and has its own TAA; **Unreal's TSR** is not open.

## Motion vectors as an engine output

Motion blur, TAA, upscalers (FSR, DLSS, XeSS all require them), temporal denoisers and
reprojection effects all consume per-pixel motion. Engines expose them as a
**velocity buffer**:

- Unreal: a velocity target, written by moving objects;
- Unity: the camera's motion vector texture;
- FSR and DLSS: RG16F input, unjittered.

The engine's buffer is a first-class output of the forward renderer with a documented
contract, so game developers can build effects on it:

- RG16F;
- the texture-coordinate delta current − previous, without jitter: the pixel's surface
  was at `uv − motion` last frame;
- objects move by the difference between last frame's transform and this frame's; the
  camera by the difference between its two matrices;
- objects without a previous transform (new, or a changed draw order) get no object
  motion.

## Conclusions

1. **Jitter:** Halton(2, 3) with ⌈8 · scale²⌉ phases (at least 8), applied to the
   projection. Motion vectors are unjittered.
2. **Motion vectors:**
   - written by the forward pass into a second target from the previous frame's
     instance transforms (kept by a per-view state, like occlusion culling's) and the
     previous camera;
   - exposed in `ForwardPasses`.
3. **TAAU resolve,** one compute pass at output resolution:
   - the current frame reconstructed from its 3 × 3 jittered samples with a
     Blackman-Harris approximation;
   - nearest-depth motion;
   - a 5-tap Catmull-Rom history (since 2026-10-10 an option, its 12 texels read one by
     one: merging them into bilinear fetches is claimed by ATI's US 8,675,007 B2, see
     `docs/research/antialiasing.md`);
   - 3 × 3 variance clipping in YCoCg on reversibly tonemapped colors;
   - the accumulated weight kept in history alpha and capped (more for static pixels);
   - history reset off screen and on camera cuts.
4. **Later, from measurements:** sharpening, locks for thin features, depth-based
   disocclusion, reactive masks for transparency (with the transparency pass), FSR 3.1
   behind the same inputs.

## Sources

- Karis, "High Quality Temporal Supersampling", SIGGRAPH 2014 Advances (https://advances.realtimerendering.com/s2014/)
- Salvi, "An Excursion in Temporal Supersampling", GDC 2016 (https://developer.download.nvidia.com/gameworks/events/GDC2016/msalvi_temporal_supersampling.pdf)
- Pedersen, "Temporal Reprojection Anti-Aliasing in INSIDE", GDC 2016; Playdead's code (MIT): https://github.com/playdeadgames/temporal
- Jimenez et al., "Dynamic Temporal Antialiasing and Upsampling in Call of Duty" (https://www.activision.com/cdn/research/Dynamic_Temporal_Antialiasing_and_Upsampling_in_Call_of_Duty_v4.pdf)
- Yang, Liu, Salvi, "A Survey of Temporal Antialiasing Techniques", EG 2020 STAR (http://behindthepixels.io/assets/files/TemporalAA.pdf)
- AMD FidelityFX Super Resolution 2 README (MIT, checked 2026-09-27): https://github.com/GPUOpen-Effects/FidelityFX-FSR2
- Bevy `bevy_anti_alias::taa` (`taa.wesl`, main branch, checked 2026-09-27): https://github.com/bevyengine/bevy/tree/main/crates/bevy_anti_alias/src/taa
- Pettineo's Catmull-Rom 9→5 tap sampling: https://gist.github.com/TheRealMJP/c83b8c0f46b63f3a88a5986f4fa982b1
