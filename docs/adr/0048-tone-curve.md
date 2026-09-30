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
- **Revisit at polish** (#71): Uchimura's curve is acceptable for now; compare the other
  curves on real content.

## Amendment (2026-09-30): AgX and ACES as options

The team accepted AgX's constants as they are and ACES under a license compatible with
MIT. Both are `ToneCurve` variants; Uchimura's curve stays the default.

- **`ToneCurve::Agx`:** Blender's inset and outset matrices for Rec.2020 (from EaryChow's
  AgX_LUT_Gen, without a license; accepted by the team), taken as Filament
  (Apache-2.0) states them, and the Rec.2020 coefficients of Wrensch's polynomial sigmoid.
  BT.709 → BT.2020 is folded into the inset matrix; after the outset and the 2.2 power the
  color returns to BT.709 and is clipped to the display (AgX leaves some colors outside
  BT.709).
- **`ToneCurve::Aces`:** Stephen Hill's fit of ACES 1.0 RRT + sRGB ODT as published in
  MJP's Baking Lab (MIT). The transforms it fits are the Academy's (a permissive
  BSD-style license; ACES 2 is Apache-2.0 under the ASWF). Notices are in
  `THIRD_PARTY_NOTICES.md`. Mid-grey maps to 0.11 (the others keep about 0.18), so ACES
  needs about +0.7 EV of exposure to match.
- **Both are SDR curves:** on HDR outputs they map to reference white and do not use the
  display's extra range. Uchimura's curve remains the HDR choice.
- **Tests:** `tests/tonemap.rs` covers all four curves on all four outputs against the CPU
  reference (`ToneCurve::apply`) on both GPUs; CPU tests check grey stays grey, monotonic,
  within [0, 1], mid-grey levels, and AgX whitening saturated highlights along their hue.

| tone map pass with bloom and exposure, Sponza 1080p, GPU µs | RTX | Radeon |
|---|---:|---:|
| Uchimura (default) | 27–31 | 993–999 |
| PBR Neutral | 22.5 | 835–846 |
| AgX | 31.7 | 985 |
| ACES | 24.6 | 905 |
