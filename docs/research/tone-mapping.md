# Tone mapping for SDR and HDR displays

Checked 2026-09-30. Sources at the end. No code was copied.

## Why

The engine tone-mapped with Khronos PBR Neutral (ADR 0014), moved to the display's peak
for HDR output (ADR 0038). PBR Neutral was designed for product visualization:
- it keeps material colors under neutral light;
- it does not aim for a filmic image, in which bright light whitens and mid-tones keep
  their contrast.

Games want the latter, in SDR and on HDR displays alike.

## Candidates, and what they cost to adopt

- **ACES (RRT + ODT, and the Narkowicz / Hill fits).**
  - The reference in film; HDR output transforms for 1000–4000 nits.
  - Known hue shifts ("notorious six") and oversaturated highlights.
  - Its code and data are under the Academy's own license (BSD-like, with conditions).
    That is not on our allowed list, so it is **a question for the team** (CLAUDE.md §15).
- **AgX** (Troy Sobotka; Blender's default since 4.0, used by Godot, three.js, Filament,
  Bevy):
  - a log2 encoding in an "inset" space, a sigmoid, an "outset" back, so bright light goes
    smoothly to white without hue skews.
  - Its constants (the inset/outset matrices) come from Blender's AgX configuration.
    Filament (Apache-2.0) takes them from EaryChow/AgX_LUT_Gen, **a repository without a
    license**, and its polynomial sigmoid from a blog post with no stated license.
  - Adopting AgX's exact look needs **the team to clear those constants' provenance**.
- **Tony McMapface** (Tomasz Stachowiak, MIT; Bevy's default):
  - a 48³ LUT from a spectral simulation, SDR only;
  - it would mean shipping a binary LUT and a separate HDR path.
- **Uchimura's curve** (Gran Turismo Sport, CEDEC 2017, "HDR Theory and Practice"):
  - a published formula;
  - a toe, a linear section (contrast a, from m over l of the range), and an exponential
    shoulder reaching the peak P;
  - P is the display's peak over paper white, so the same curve serves SDR (P = 1) and
    HDR with the same mid-tones;
  - applied per channel, bright colors whiten as their channels reach P.
- **Lottes 2016** (AMD, "Advanced Techniques and Optimization of HDR Color Pipelines"): a
  parametric curve with HDR peak control and saturation handling; a published formula,
  another candidate.
- **Hue-preserving mapping in a perceptual space** (Oklab, Ottosson, public domain / MIT;
  or ICtCp as in Frostbite, Fry 2017): tone-map lightness, then reduce chroma near white.
  Clean to adopt, but its look needs artistic tuning.

## Conclusions

1. **Uchimura's curve becomes the default:**
   - license-clean (a formula);
   - HDR-native (P);
   - mid-tones exactly kept (a = 1: 0.18 stays 0.18 in SDR and HDR);
   - a filmic shoulder with a path to white per channel;
   - the published defaults (a 1, m 0.22, l 0.4, c 1.33, b 0).
2. **Applied per channel in BT.709:**
   - tone-mapping in BT.2020 and converting back exceeds the peak for saturated colors
     (pure red maps to 1.66 P);
   - BT.709 keeps every channel within [0, P].
3. **PBR Neutral stays as `ToneCurve::PbrNeutral`** for product and material views.
4. **AgX and ACES as options** (decided by the team on 2026-09-30, ADR 0048's
   amendment): AgX's constants accepted as they are; ACES under the Academy's permissive
   license (ACES 2: Apache-2.0), through Hill's fit in MJP's Baking Lab (MIT).

## Sources

- Uchimura, Suzuki, "Practical HDR and Wide Color Techniques in Gran Turismo SPORT", CEDEC 2017 / SIGGRAPH Asia 2018 course
- Khronos, "PBR Neutral Tone Mapping" specification
- Filament, `filament/src/ToneMapper.cpp` (AgX section and its attributions), main, checked 2026-09-30 (Apache-2.0)
- EaryChow, AgX_LUT_Gen (https://github.com/EaryChow/AgX_LUT_Gen), no license file, checked 2026-09-30
- MJP and David Neubelt, Baking Lab, `BakingLab/ACES.hlsl` (Stephen Hill's fit), master, checked 2026-09-30 (MIT)
- A.M.P.A.S., aces-dev `LICENSE.md` (dev branch), checked 2026-09-30; aces-aswf/aces-core `LICENSE` (Apache-2.0), checked 2026-09-30
- Ottosson, "A perceptual color space for image processing" (Oklab; public domain or MIT), checked 2026-09-30
- Lottes, "Advanced Techniques and Optimization of HDR Color Pipelines", GDC 2016
- Fry, "High Dynamic Range Color Grading and Display in Frostbite", GDC 2017
