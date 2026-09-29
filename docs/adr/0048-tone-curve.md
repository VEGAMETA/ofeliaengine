# 0048. The tone curve: Uchimura's by default, PBR Neutral as an option

- Status: Accepted
- Date: 2026-09-30
- Builds on: [ADR 0038](0038-hdr-output.md) (HDR output and the peak),
  [ADR 0047](0047-bloom-and-automatic-exposure.md) (exposure before the curve)
- Research: [`docs/research/tone-mapping.md`](../research/tone-mapping.md)

## Context

The tone mapper was Khronos PBR Neutral moved to the display's peak, planned as
temporary. It is meant for product visualization, not a filmic image. The replacement
must serve SDR and HDR displays, keep mid-tones, and be license-clean.

## Options

1. **ACES:** the film reference with HDR output transforms, but hue shifts and the
   Academy's license (not on the allowed list).
2. **AgX:** the modern default in Blender, Godot and Filament. Its constants come from a
   repository without a license. Needs the team to clear their provenance.
3. **Tony McMapface:** MIT, but a binary LUT and SDR only.
4. **Uchimura's curve (Gran Turismo Sport) (chosen):**
   - a published formula with a peak parameter;
   - linear mid-tones;
   - an exponential shoulder;
   - per channel, a path to white for bright colors.
5. A hue-preserving curve in Oklab: clean, but a look to design and tune without art
   direction.

## Decision

- **`post::ToneCurve { Gt (default), PbrNeutral }`**, chosen per composite
  (`Composite::curve`).
- **Uchimura's curve**, the published defaults (a 1, m 0.22, l 0.4, c 1.33, b 0):
  - applied per channel in BT.709, after exposure and bloom;
  - the peak is the output's (1 for SDR, `OutputLevels::peak()` for HDR);
  - HDR10 converts to BT.2020 after the curve, as before.
  - In BT.2020 the conversion back would exceed the peak for saturated colors (pure
    red: 1.66 × P).
- **The shader evaluates only the section a value falls in:**
  - toe (blended into the linear section), linear, or shoulder;
  - neighbouring pixels share it, so the power and the exponential are not both paid for.
- **`ToneCurve::apply`** is the CPU description (tests, tools).
- `scene_bench --curve gt|pbr-neutral`; screenshots go through the same curve, with
  `--post` also bloom and exposure.

## Measurements

**Correctness:**
- `tests/tonemap.rs`: both curves, through SDR, scRGB with a 5× peak, and HDR10 with the
  curve in the same pass and without it, match the CPU description within float rounding
  on both GPUs (96 values).
- CPU tests of Uchimura's curve at peaks 1, 2.5, 5 and 12.5:
  - 0.5 stays 0.5;
  - monotonic, never above the peak, reaching it far above;
  - continuous in value and slope where the shoulder starts;
  - black stays black;
  - bright colors whiten.

**Cost:** the tone map pass with bloom and exposure, Sponza at 1080p, GPU µs:

| | RTX 4060 Ti | Radeon iGPU |
|---|---:|---:|
| PBR Neutral | 22.5 | 879 |
| Uchimura, every section evaluated | 31.7 | 998 |
| **Uchimura, only its section (chosen)** | 27.6 | 989 |

## Consequences

- **New projects get a filmic look** with the same mid-tones in SDR and HDR, and a
  display's full range on HDR screens. PBR Neutral remains for material viewers.
- **Not done yet:**
  - a hue-preserving path to white (inset and outset, as AgX does), which removes the
    hue skews of per-channel curves on saturated highlights;
  - color grading (LUTs).
- **Questions for the team:**
  - AgX, if its constants' provenance is cleared;
  - ACES, if the Academy's license is acceptable.
