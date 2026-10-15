# Color grading

Checked 2026-10-15. Sources at the end. No code was copied: the operations below are
published standards (ASC CDL, CIECAT16, ACEScct) and common practice, and Filament's
code (Apache-2.0) was read for their order and defaults, credited where taken.

## Why

The tone mapper gives every project the same look (ADR 0048). Games change it: a warm
evening, a cold night, desaturated flashbacks, the contrast of a horror game. Colorists
know a few standard controls from DaVinci Resolve, Unreal and Unity, and artists bring
LUTs made in those tools. Queue item 6a.

## How others do it

- **Filament** (`ColorGrading.h`, `details/ColorGrading.cpp`, main, Apache-2.0):
  - one 3D LUT holds the whole chain: grading, the tone mapper, the output's transfer
    function. The post-process pass encodes the HDR color, samples the LUT once and
    writes the result;
  - the LUT is indexed by a log encoding (LogC) of scene-linear values, 32³ by default
    (16³ to 64³; 10-bit integer or half float), built on the CPU when the settings change
    (a slice per job above 32³);
  - the order: exposure, night adaptation, white balance (von Kries in CIECAT16,
    temperature and tint each in −1..1 moving the white point along and across the
    daylight locus), channel mixer, shadows/mid-tones/highlights (color multipliers
    weighted by smoothsteps of linear luminance over four "tonal range" points, Resolve's
    defaults 0, 0.333, 0.55, 1), then in log space (ACEScct-like) the ASC CDL
    (slope, offset, power) and contrast around mid-grey (Resolve's convention), back to
    linear for vibrance and saturation, curves, tone mapping, gamut mapping, the OETF; an
    optional custom LUT (LDR, sRGB) last;
  - grading works in Rec.2020 (white balance and the mixer change the gamut);
  - a 1D LUT (512 entries) replaces the 3D one when only a per-channel tone mapper runs.
- **Unreal** (documentation only; its source is not open-source licensed):
  - Temperature/tint white balance, then Global, Shadows, Midtones and Highlights
    groups, each with saturation, contrast, gamma, gain and offset (RGB and luminance),
    the ranges split at `ShadowsMax` (0.09) and `HighlightsMin` (0.5);
  - all of it and the filmic tone mapper "combined" into one 32³ volume LUT in log
    space every frame the settings change; the tone mapper samples it;
  - an artist LUT (16³ as a 256 × 16 strip, authored on a screenshot) is folded into
    the same volume. It is display-referred, made for SDR.
- **Bevy** (`bevy_core_pipeline/src/tonemapping.wesl`, `bevy_render::view::ColorGrading`,
  main, MIT or Apache-2.0):
  - evaluated per pixel in the tone mapper, each part behind a shader define (hue
    rotation, white balance, "sectional" grading);
  - white balance as a 3 × 3 LMS matrix computed on the CPU from temperature and tint;
  - sections by the mean of R, G and B with Blender compositor's margins; per section
    saturation, contrast (around 0.5), gamma, gain and lift as the ASC CDL formula, and
    exposure;
  - a post-tone-mapping saturation.
- **Godot** (`renderer_rd/shaders/effects/tonemap.glsl`, master, MIT): brightness in
  linear, then contrast and saturation on the sRGB-encoded result, then an optional color
  correction texture (1D gradient or 3D LUT) sampled with the sRGB-encoded color.
  Display-referred only.
- **Frostbite** (Fry, GDC 2017): grading in a display-independent space (PQ-encoded,
  ICtCp), so one grade serves SDR and HDR displays; the LUT is authored for that space.

## What matters for us

- **The tone mapper must stay what it is when nothing is graded.** Its curves are exact
  (`tests/tonemap.rs` compares them with `ToneCurve::apply` within float rounding), and a
  feature costs nothing where it is not used (CLAUDE.md §5): grading runs in entry points
  of its own.
- **One fetch per pixel.** Folding grading and the curve into one 3D LUT (Filament,
  Unreal) makes the graded tone mapper as cheap as the plain one whatever is graded, and
  an artist LUT folds into the same volume later.
- **Precision of a LUT that holds the curve.** Interpolating Uchimura's curve (the
  default) through a LUT indexed by ACEScct, against the curve itself, in 8-bit sRGB
  (a script over 300 values per octave):

  | LUT | SDR, worst | HDR (peak 12.5), worst |
  |---|---:|---:|
  | 32³ | 1.38 / 255 | 1.28 / 255 |
  | 48³ | 0.64 / 255 | 0.59 / 255 |
  | 64³ | 0.36 / 255 | 0.33 / 255 |

  The worst error sits where the shoulder starts (its second derivative jumps). Plain
  log2 encodings over −12..+10 or −10..+8 EV around mid-grey do worse than ACEScct at the
  same size (32³: 2.3 and 1.6 / 255), the first because it spends entries on darks
  nobody sees. A 48³ LUT stays under one 8-bit step.
- **HDR outputs.** The curve depends on the output's peak, so the LUT is built for it
  (rebuilt when the output changes, which is rare). Scene-referred grading before the
  curve is display-independent, as Frostbite wants. An artist's display-referred LUT made
  on an SDR screenshot has no meaning above reference white: it applies to SDR outputs.
- **Building the LUT** on the GPU (a compute pass over the N³ texels, only when the
  settings, the curve or the output change) keeps animated grading (damage, time of day)
  off the CPU; the curve's code is shared with the tone mapper.

## Conclusions

1. **A combined LUT** of the grading and the tone curve: indexed by the ACEScct encoding
   of the scene-linear color after exposure and bloom, measured from black (black on the
   first texel); tone-mapped display-linear output in units of reference white (the
   output's scale and PQ stay in the shader). 48³ RGBA16F, 864 KiB. Built by a compute
   pass (`grading.slang`, the curves' code shared with the tone mapper through
   `tone_curves.slang`) when the grading, the curve or the peak change. **PBR Neutral is
   the exception:** it couples the channels (its offset follows the darkest, its roll-off
   the brightest), which the LUT interpolated with up to 6.3 / 255 of error; with it the
   LUT holds the graded color in ACEScct, and the shader's curve follows.
2. **Graded entry points** beside the plain ones (main, perceptual, Catmull-Rom,
   Lanczos-3): the plain tone mapper keeps its code, registers and exact curves.
3. **The operations, in Filament's order** (the standard controls, credited):
   - white balance (CIECAT16 von Kries, temperature and tint along and across the
     daylight locus) and the channel mixer, folded on the CPU into one 3 × 3 matrix;
   - shadows, mid-tones and highlights (color multipliers over Resolve's tonal ranges);
   - in ACEScct: the ASC CDL (slope, offset, power) and contrast around mid-grey;
   - in linear: vibrance and saturation;
   - then the tone curve.
   Working space BT.709 linear (the tone curves' space, ADR 0048).
4. **A CPU reference** of the chain (`ColorGrading::apply`) for tests and tools, as
   `ToneCurve::apply` is for the curves.
5. **Later:** artists' `.cube` LUTs (display-referred, SDR outputs only), folded into the
   same LUT; hue rotation; curves.

## Measured (2026-10-15, ref-1)

- **Precision on the GPU** (`graded_tone_map_matches_the_reference`):
  - The test sends 12 values, from black to 100 and saturated colors, through SDR, scRGB
    (1000-nit peak) and HDR10 outputs, with every curve and with a neutral and a strong
    grading.
  - Its worst error against `curve(grading(x))`, compared as the display shows it: 0.85 /
    255 on the RTX and 0.84 / 255 on the Radeon.
  - With the coordinate starting at ACEScct's 0, black fell between texels. The clamped
    texels below it lifted HDR10's black by 7.0 / 255. Measured from black, black sits on
    the first texel.
  - The grading alone in a 32³ LUT before the exact curve: 0.76 / 255.
- **Cost:** the tone map pass on Sponza at 1080p, with bloom and automatic exposure.
  `scene_bench --sky --post` against `--grading` (a grading touching every control),
  alternated:

  | | RTX 4060 Ti | Radeon iGPU |
  |---|---:|---:|
  | plain | 28.7–30.7 µs | 1,122–1,135 µs |
  | the grading alone in the LUT, the curve after it (32³) | 44.0 µs | 1,510–1,516 µs |
  | grading and curve in one LUT, 32³ | 43.0 µs | 1,198–1,202 µs |
  | grading and curve in one LUT, 48³ (chosen) | 44.0 µs | 1,215–1,219 µs |
  | PBR Neutral: plain → graded | 24.6 → 44.0 µs | 942 → 1,338 µs |

  - The RTX pays for the dependent 3D fetch (about 14 µs), whatever the LUT holds.
  - The Radeon pays for the arithmetic after the fetch: the decode and the curve cost it
    another 0.3 ms.

## Sources

- Filament, `filament/include/filament/ColorGrading.h` and `filament/src/details/ColorGrading.cpp`, main, checked 2026-10-15 (Apache-2.0)
- Bevy, `crates/bevy_core_pipeline/src/tonemapping.wesl`, `crates/bevy_render/src/view/mod.rs` (`ColorGrading`), main, checked 2026-10-15 (MIT OR Apache-2.0)
- Godot, `servers/rendering/renderer_rd/shaders/effects/tonemap.glsl`, master, checked 2026-10-15 (MIT)
- Unreal Engine documentation, `FColorGradingSettings` and post-process "Color Grading", 4.27 / 5.x, checked 2026-10-15
- Fry, "High Dynamic Range Color Grading and Display in Frostbite", GDC 2017
- ASC Color Decision List (ASC CDL) Transfer Functions and Interchange Syntax, v1.2
- Academy S-2016-001, "ACEScct — A Quasi-Logarithmic Encoding of ACES Data for use within Color Grading Systems"
- Li, Li, Wang, Luo, Cui, Melgosa, Brill, Pointer, "Comprehensive color solutions: CAM16, CAT16, and CAM16-UCS", Color Research & Application 42 (6), 2017
