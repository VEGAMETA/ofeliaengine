# 0097. Color vision deficiencies: simulated and corrected in the graded LUT

- Status: Accepted
- Date: 2026-10-15
- Research: [docs/research/color-vision.md](../research/color-vision.md)
- Related:
  - [ADR 0094](0094-color-grading.md): the graded LUT this rides on;
  - [ADR 0048](0048-tone-curve.md): the tone curves;
  - queue item 8e (the owner, 2026-10-14: a light addition for the game developer).

## Context

About one man in twelve sees red and green less apart than others do. Games give such
players a correction filter, and their developers a simulation to check that nothing
essential is told by color alone.

The owner asked for both, as a light addition that costs nothing while off.

## Options

1. **A matrix in the tone mapper**, in entry points of their own (twins of the plain and
   graded ones).
   - Exact.
   - Every tone mapper variant gets a twin (more than a dozen).
2. **In the grading LUT, after the tone curve** (Unreal's place for it).
   - A view with it tone maps through the graded path, sampling the LUT once per pixel.
   - Nothing changes in the shaders of the views without it.
3. **A pass of its own after the tone mapper.** A full-screen read and write: on the iGPU
   about 1 ms at 1080p for a 3 × 3 matrix.

## Decision

**Option 2.**

**`engine_render::color_vision::ColorVision`:**

| field | values |
|---|---|
| `deficiency` | `Protan`, `Deutan` or `Tritan` (or none) |
| `severity` | 0–1: anomalous trichromacy up to dichromacy |
| `mode` | `Correct` (the player's filter) or `Simulate` (the developer's check) |

- **The default is off**, and off costs nothing: `is_off()` keeps the plain tone mapper.
- **Simulation, protan and deutan:** Machado, Oliveira and Fernandes's matrices (2009),
  from their published table at severities 0, 0.1, …, 1, interpolated between steps.
- **Simulation, tritan:** Brettel, Viénot and Mollon's two half-planes (1997), in linear RGB
  with libDaltonLens's constants (public domain), mixed with the original color by the
  severity. Machado's model was not made for tritans.
  - The half-planes meet on a separation plane through the grey axis, where the projections
    agree but their slopes do not.
  - The LUT interpolating across that kink brought greys out up to 3 / 255 tinted.
  - So the two projections are blended where `dot(n, c) / (r + g + b)` is within 0.02.
    That departs from either projection by at most about 2 % of `r + g + b`, and greys come
    through within 1.55 / 255.
- **Correction:** daltonization (Fidaner, Lin and Ozguven 2005):
  `c + E (c − sim c)`, never below 0.
  - For protans and deutans, E moves the lost red-green difference into green and blue:
    `[[0, 0, 0], [0.7, 1, 0], [0.7, 0, 1]]`.
  - For tritans, E moves the lost blue into red and green.
- **The LUT holds `transform(curve(grading(x)))`.**
  - With PBR Neutral, whose curve follows the LUT (ADR 0094), the transform comes before
    the curve. That is the same below its roll-off (0.76 of white), where PBR Neutral
    leaves colors as they are but for its toe's offset.
  - The LUT's key and parameters carry the transform; it is built again when it changes.
- **`engine_frame`:** `ViewSettings::color_vision`.
  - A view with a deficiency set tone maps through its grading LUT, made the first frame it
    needs one and kept.
- **Patents** (research note):
  - Apple's US 9,984,658 expired.
  - NVIDIA's US 12,620,145 claims luminance-preserving remapping into a CVD gamut. We work
    in linear RGB and do not preserve luminance.

## Measurements

- **Cost:** off, nothing. On, the graded tone mapper's (ADR 0094, Sponza at 1080p): RTX
  29–31 → 44 µs, Radeon 1.12 → 1.22 ms. The LUT (48³) is built only in the frames where
  the transform, the grading, the curve or the peak change.
- **Accuracy of the LUT against the CPU description**, every output and curve on both GPUs
  (`color_vision_tone_map_matches_the_reference`):
  - protan and deutan within 1 / 255 of 8-bit output, as the graded path;
  - tritan within 2 / 255 (1.55 measured, on greys).
- **Tests:**
  - **CPU unit tests:**
    - off changes nothing;
    - the published table is reproduced;
    - greys stay grey;
    - for each deficiency, two colors along the dichromat's confusion direction look the
      same simulated, and are told apart after correction (more than 5 times as far
      apart).
  - **Golden image** `sponza_deuteranopia` per vendor: the red and green curtains alike,
    the blue ones apart.
  - **Zero allocations** with a correction whose severity changes every frame. The
    validated mode switching turns a tritan simulation on and off.

## Consequences

- **Players can be offered the correction and developers the simulation**, per view, at the
  graded tone mapper's cost and only while on.
- **With PBR Neutral, the transform comes before the curve.** Above 0.76 of white it
  differs a little from one after it.
- **Not done:**
  - a luminance-preserving correction (patented, above);
  - achromatopsia (monochromacy), easy to add as a fourth kind if wanted.
