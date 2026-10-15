# Color vision deficiencies: simulation and correction

Checked 2026-10-15. Queue item 8e (the owner, 2026-10-14: "a light addition for the game
developer"). Sources at the end; no code was copied. Constants are taken from:

- the papers' published tables (Machado et al.);
- libDaltonLens, which is public domain (Brettel's tritan projections).

## What players have, and what games offer

- **About 8 % of men and 0.5 % of women** have a red-green deficiency: protan (the
  long-wavelength cones) or deutan (the medium ones).
  - It is anomalous trichromacy (protanomaly, deuteranomaly) when the cones are shifted.
  - It is dichromacy (protanopia, deuteranopia) when they are missing.
- **Tritan** deficiencies (the short-wavelength, blue cones) are rare, about 0.01 %.
- **Games offer two things:**
  - a **correction filter** the player turns on, usually with a type and a strength
    (Fortnite's color blind mode and strength, for one, through Unreal's
    `r.Color.Deficiency`);
  - a **simulation** the developer uses to check that nothing relies on color alone
    (Unreal's same setting with "simulate").
- **Accessibility guides** (Game Accessibility Guidelines, Xbox Accessibility Guideline 103)
  ask that no essential information be carried by color alone. A filter helps where a game
  falls short of that, and the simulation is how developers find where they rely on color.

## How others do it

### Unreal Engine 5 (documentation only; its code is under Epic's EULA)

- `r.Color.Deficiency.Type` (deuteranope, protanope, tritanope), `.Severity` (0–10),
  `.CorrectDeficiency` (simulate or correct) and `.ShowCorrectionWithDeficiency`.
- It acts on the displayed color, after the tone curve. Which model it uses is not ours to
  read (the code is under the EULA) and its documentation does not say.

### Machado, Oliveira and Fernandes 2009 (IEEE TVCG)

- **The model:** the anomalous cone's sensitivity is shifted towards its neighbour's.
  Severity 0–1, where 1 is dichromacy. The paper and its page give the resulting 3 × 3
  matrices in linear RGB, at severities 0, 0.1, …, 1.
  - Between the table's steps, the authors suggest interpolating linearly.
  - Matrices applied to gamma-encoded values (a frequent mistake) darken the image; they
    belong in linear RGB.
- **Its limits** (DaltonLens's comparison):
  - for dichromacy it ranks a little below Viénot 1999 and Brettel 1997;
  - for tritans it was neither designed nor validated (the authors did not model
    tritanopia), and it does poorly there.

### Brettel, Viénot and Mollon 1997 (JOSA A); Viénot, Brettel and Mollon 1999

- **Dichromacy as a projection in LMS space** along the missing cone's axis.
  - Brettel projects onto two half-planes, each spanned by the neutral axis and an anchor
    wavelength: 475 and 575 nm for protans and deutans, 485 and 660 nm for tritans.
  - Which half-plane a color uses depends on its side of a separation plane.
- **Viénot 1999** simplifies Brettel to one plane for protans and deutans. That is wrong
  for tritans.
- **libDaltonLens** (public domain) gives Brettel's tritan projections as two matrices in
  linear RGB and the separation plane's normal, derived from the LMS model.
  - For a severity below 1, it mixes the simulated color with the original.

### Daltonization: Fidaner, Lin and Ozguven 2005

- **Correction** = the color plus its error against the simulation, moved into channels
  the viewer keeps.
  - The error matrix shifts the red-green error into green and blue:
    `[[0, 0, 0], [0.7, 1, 0], [0.7, 0, 1]]`.
  - Linear throughout: with a matrix simulation, the whole correction is one 3 × 3 matrix,
    `I + E (I − S)`.
- daltonize.org's and the Godot community shader's correction is this one.
- Variants weigh the shift differently. None is universally best; the effect is that
  confused pairs become distinguishable, at the cost of their exact hue.

## Patents (checked 2026-10-15, targeted searches)

- **Apple US 9,984,658 B2, "Displays with improved color accessibility"** (2016): expired
  (fee related, Google Patents). Its claims required content-specific or regional strength
  (claims 1 and 11), or a stored 3D LUT with varying strengths (claim 17).
- **NVIDIA US 12,620,145 B2, "Luminance-preserving and temporally stable daltonization"**
  (priority 2022-12, granted 2026-05-05, active to 2044). Every independent claim needs
  either of two things:
  - a projection into a region "within a luminance polygon" or "a displayable CVD gamut" of
    a luminance-based color space, then a CVD-constrained remapping (claims 1, 6, 7, 9, 11,
    17, 18);
  - a remapping that keeps each color's luminance, consistent across a sequence (12, 19,
    20).

  The classic method does neither:
  - it works in linear RGB;
  - it adds the error back, so it does not preserve luminance;
  - it does not constrain colors to the dichromat's gamut.

  So we keep to the classic method and never preserve luminance by construction. Not
  infringing.
- **Samsung US 9,966,013 B2** (2018): a display apparatus with a backlight driven by a
  correction matrix (hardware claims). Not ours.
- **US 12,367,619** (2025): spectral representations shifted by transformation curves. Not
  ours.
- **None found** on simulation matrices or daltonization in games. These have been
  published since 1997–2009 and are widely used in games and mods.

## What matters for us

- **Where.** On the displayed color: linear BT.709 after the tone curve, before the output
  encoding (sRGB or PQ).
  - The tone mapper's graded path already samples a 3D LUT that holds the grading and the
    curve (ADR 0094). The transform goes into that LUT after the curve, at no cost per
    pixel beyond the LUT lookup.
  - A view without it keeps the plain tone mapper: nothing to pay, not even registers.
  - **PBR Neutral** is the exception: its curve follows the LUT, because a LUT interpolates
    its coupled channels badly. The transform then comes before the curve. Up to PBR
    Neutral's roll-off (0.76 of white) the curve leaves colors as they are, but for its
    toe's offset, so before and after agree there.
- **Which models.**
  - **Protan and deutan:** Machado's matrices at all severities, interpolated. At 1 they
    give dichromacy, close to Viénot's.
  - **Tritan:** Brettel's two half-planes (libDaltonLens's constants), mixed with the
    original color by the severity.
- **A LUT across Brettel's kink.** The two half-planes meet on their separation plane, which
  holds the grey axis. Their projections agree on the plane, but the change of slope there
  made the LUT bring greys out up to 3 / 255 tinted.
  - The fix: blend the two projections over a narrow band, `dot(n, c) / (r + g + b)` within
    0.02. The blend departs from either projection by at most about 2 % of `r + g + b`.
  - Greys then come through within 1.55 / 255.
- **Correction:** daltonization with Fidaner's shift for protans and deutans. For tritans,
  the blue error is shifted into red and green, the same rule for the blue channel.
- **API:**
  - `ColorVision { deficiency, severity, mode: Correct | Simulate }`, in
    `engine_render::color_vision`;
  - `ViewSettings::color_vision` in `engine_frame`.
- **Tests:**
  - **CPU:** off does nothing; the published table is reproduced; greys stay grey; a pair
    the simulated dichromat confuses (along the projection's null direction) becomes
    distinguishable after correction.
  - **GPU:** the tone mapper through the LUT matches the CPU description on every output and
    curve.
  - **Images:** a golden image of Sponza as a deuteranope sees it.
  - **Allocations:** none, with the correction animated.

## Sources

- G. M. Machado, M. M. Oliveira, L. A. F. Fernandes, "A Physiologically-based Model for Simulation of Color Vision Deficiency", IEEE TVCG 15(6), 2009; matrices at https://www.inf.ufrgs.br/~oliveira/pubs_files/CVD_Simulation/CVD_Simulation.html (checked 2026-10-15)
- H. Brettel, F. Viénot, J. D. Mollon, "Computerized simulation of color appearance for dichromats", JOSA A 14(10), 1997
- F. Viénot, H. Brettel, J. D. Mollon, "Digital video colourmaps for checking the legibility of displays by dichromats", Color Research & Application 24(4), 1999
- O. Fidaner, P. Lin, N. Ozguven, "Analysis of Color Blindness", Stanford, 2005 (daltonization)
- N. Burrus, "Review of Open Source Color Blindness Simulations", daltonlens.org (2021, updated), and libDaltonLens (`libDaltonLens.c`, Unlicense), github.com/DaltonLens/libDaltonLens (checked 2026-10-15)
- Unreal Engine 5 documentation: `r.Color.Deficiency.*` console variables (checked 2026-10-15)
- Godot shaders: "Colorblindness correction shader" (godotshaders.com), daltonize.org's algorithm
- Game Accessibility Guidelines (gameaccessibilityguidelines.com), Xbox Accessibility Guideline 103 (color)
- Patents: US 9,984,658 B2 (Apple, expired), US 12,620,145 B2 (NVIDIA), US 9,966,013 B2 (Samsung), US 12,367,619 (Google Patents, checked 2026-10-15)
