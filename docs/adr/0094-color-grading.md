# 0094. Color grading: the grading and the tone curve in one LUT

- Status: Accepted
- Date: 2026-10-15
- Research: [docs/research/color-grading.md](../research/color-grading.md)
- Related: [ADR 0048](0048-tone-curve.md) (the tone curves), [ADR 0038](0038-hdr-output.md)
  (HDR output), [ADR 0047](0047-bloom-and-automatic-exposure.md) (exposure before the
  curve); queue item 6a

## Context

- Every project gets the same look from the tone curve. Games change it: a warm evening,
  a cold night, desaturated flashbacks, a horror game's contrast.
- Colorists know a few standard controls (DaVinci Resolve, Unreal, Unity): white balance,
  shadows, mid-tones and highlights, lift / gamma / gain (the ASC CDL), contrast,
  vibrance, saturation.
- The tone mapper's curves are exact. `tests/tonemap.rs` compares them with
  `ToneCurve::apply` within float rounding. A feature must cost nothing where it is not
  used (CLAUDE.md §5).

## Options

1. **Grading evaluated per pixel in the tone mapper** (Bevy). It is exact, and each part
   is a shader variant. The cost grows with what is graded: the CDL and contrast need a
   log space, so three `log2` and three `exp2` per pixel, plus `pow` for the CDL's power.
2. **The grading alone in a 3D LUT before the analytic curve.** The LUT is indexed by
   ACEScct and holds ACEScct, so the curves stay exact. Measured on Sponza at 1080p, the
   tone map pass with bloom and exposure: RTX 28–31 → 44 µs, Radeon 1,129–1,135 →
   1,510–1,516 µs. The LUT's decode and the curve's own transcendental functions cost the
   iGPU dearly.
3. **The grading and the curve in one 3D LUT** (Filament, Unreal). The graded tone mapper
   encodes the color, samples the LUT once and writes. RTX 29–31 → 44 µs, Radeon 1,122 →
   1,215–1,219 µs. A LUT interpolates badly where a curve couples the channels: PBR Neutral's
   offset follows the darkest channel and its roll-off the brightest, and it drifted by up
   to 6 / 255 there. Uchimura's curve, AgX and ACES stay under 1 / 255 at 48³.

## Decision

**Option 3, with PBR Neutral handled as option 2:**

- **`engine_render::grading::ColorGrading`** holds the controls; its default changes
  nothing. They apply in Filament's order (Apache-2.0, credited; the operations are
  published standards), in linear BT.709:
  1. white balance (von Kries in CIECAT16, temperature and tint in −1..1 moving the
     scene's white point along and across the daylight locus) and the channel mixer,
     folded on the CPU into one matrix;
  2. shadows, mid-tones and highlights: color multipliers weighted by smoothsteps of
     luminance over four tonal points (Resolve's defaults 0, 0.333, 0.55, 1);
  3. in ACEScct: the ASC CDL (slope, offset, power) and contrast around mid-grey
     (Resolve's convention);
  4. vibrance (saturation scaled by how unsaturated a color is) and saturation, around
     luminance.

  `ColorGrading::apply` is the CPU description.
- **`GradingLut`**: 48³ RGBA16F (864 KiB) per grading view.
  - It is indexed by the ACEScct encoding of the exposed scene-linear color, measured
    from black. Black sits on the first texel: between texels, the clamped values below
    black lifted it, by 7 / 255 on an HDR10 output.
  - It holds the tone-mapped color in units of reference white. With PBR Neutral it holds
    the graded color in ACEScct, and the tone mapper's curve follows.
  - A compute pass (`gradeLut`, sharing `tone_curves.slang` with the tone mapper) builds
    it in the frames whose grading, curve or peak changed.
- **The tone mapper's graded entry points** (`fragmentGraded` and the perceptual and
  resampling twins) are pipelines described beside the plain ones, made on first use or
  by the device's warm-up thread (ADR 0079).
  `Composite::grading` selects them. The plain entry points' code is unchanged: their
  SPIR-V differs only in the push constants' two new members.
- **`engine_frame`:** `ViewSettings::grading`. A view makes its LUT the first frame it
  grades and keeps it while it lives. A grading animated back to neutral and away again
  (a damage flash) makes nothing again, and a neutral grading takes the plain path.

## Measurements

- **Precision.** `graded_tone_map_matches_the_reference` sends 12 scene values, from
  black to 100 and saturated colors, through every curve. It covers SDR, scRGB (1000-nit
  peak) and HDR10 tone-mapped in the pass, with a neutral and a strong grading. The result
  matches `curve(grading(x))` within 0.85 / 255 on the RTX and 0.84 / 255 on the Radeon,
  compared as a display shows it. Its LUT at 32³ reached 0.76 / 255 in option 2.
- **Cost.** The tone map pass, Sponza, 1080p:

  | | RTX 4060 Ti | Radeon iGPU |
  |---|---:|---:|
  | `scene_bench --sky --post` → `--grading` (bloom, exposure) | 28.7–30.7 → 44.0 µs | 1,122–1,123 → 1,215–1,219 µs |
  | the same with PBR Neutral | 24.6 → 44.0 µs | 942 → 1,338 µs |
  | `perf_gate`: `sponza` → `sponza_graded` | 26.6 → 44.0 µs | 866 → 1,032 µs |
  | a 32³ LUT instead | 43.0 µs | 1,198–1,202 µs |

  - On the RTX the graded pass costs its dependent 3D fetch, whatever the LUT holds.
  - The plain tone mapper is unchanged: `sponza` 26.6 µs on the RTX against its baseline.
  - The LUT's build is one dispatch of 48³ threads, only when the settings change.
- **Golden image:** `sponza_graded` per vendor.

## Consequences

- Grading costs a view nothing until it grades, then 13–15 µs on the RTX and 0.1–0.17 ms
  on the Radeon at 1080p, whatever is graded. With PBR Neutral, whose curve stays in the
  shader, it costs 19 µs and 0.4 ms.
- **Animated grading** (damage, time of day) rebuilds the LUT in each frame it changes,
  with one dispatch.
- **Later:**
  - artists' `.cube` LUTs (display-referred, SDR outputs), folded into the same LUT;
  - hue rotation;
  - curves.
