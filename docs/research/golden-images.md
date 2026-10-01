# Golden images and GPU time gates

Research for regression checks of rendering: what a render is compared with, how, and how
per-pass GPU time is guarded. Decision: [ADR 0062](../adr/0062-golden-images-and-perf-gate.md).

Checked on 2026-10-01. FLIP's reference code (BSD-3) was read and ported; engines' test
setups are described from their public documentation.

## Sources

| Source | Version | License | What we take |
|---|---|---|---|
| P. Andersson, J. Nilsson, T. Akenine-Möller, M. Oskarsson, K. Åström, M. D. Fairchild, "FLIP: A Difference Evaluator for Alternating Images", HPG 2020 | — | paper | the metric: CSF filtering in YCxCz, HyAB in Hunt-adjusted L\*a\*b\*, edge and point features, `color^(1-feature)` |
| P. Andersson, J. Nilsson, T. Akenine-Möller, "Visualizing and Communicating Errors in Rendered Images", Ray Tracing Gems II, 2021 | — | book chapter | how to read FLIP maps and means; mean plus percentiles as summaries |
| NVIDIA, `NVlabs/flip`, `src/cpp/FLIP.h`, `src/tests/test.py` | main, 2025 | BSD-3-Clause | constants, separable filters, the published mean 0.159691 of its test pair |
| Z. Wang et al., "Image Quality Assessment: From Error Visibility to Structural Similarity" (SSIM), IEEE TIP 2004 | — | paper | the alternative, and why it was not chosen |
| Unreal Engine "Screenshot comparison" / Gauntlet automation, Unity Graphics Test Framework | public docs | products | per-platform (per-GPU) reference images, tolerance per test, a report with the diff |
| Chromium / Skia Gold, Mozilla reftests | public docs | open source | per-configuration baselines, triage of intended changes |

## Comparing renders

- **Exact comparison** works on one GPU and driver when the renderer is deterministic
  (ours is: two runs give FLIP 0). A driver update or another GPU changes low bits
  everywhere, so hashes cannot be the gate.
- **PSNR / RMSE / max difference** treat all channels and positions alike: a +3 %
  exposure (a mean FLIP of 0.04–0.06 here) and a small displaced shadow can score alike.
- **SSIM** captures structure but is built on grey levels and local windows; calibration
  for "would I notice when flipping" is FLIP's purpose.
- **FLIP** models a viewer flipping between the two images at a distance (pixels per
  degree; the default 67 is 0.7 m from a 0.7 m wide 4K monitor):
  - colour: YCxCz, blurred by the contrast sensitivity functions of the achromatic, red-
    green and blue-yellow channels; back to linear RGB, clamped; L\*a\*b\* with Hunt's
    adjustment; the HyAB distance (city-block in L, Euclidean in a, b), raised to 0.7 and
    remapped so the largest possible distance (green against blue) is 1;
  - features: first and second derivatives of a Gaussian on the luminance, edges and
    points in one image and not the other;
  - per pixel `color^(1 - feature)`: a feature difference amplifies a colour difference.
- FLIP needs no training data and no GPU; our port runs a 1080p pair in 91 ms.

## References per GPU

Rendering is not bit-identical across vendors: texture filtering (anisotropy, LOD),
block-compressed texture decoding, rasterization rules at edges and alpha-test coverage
differ within the specs' tolerances. Measured between the RTX 4060 Ti and the Radeon iGPU,
mean FLIP 0.002–0.013 (99.9th percentile 0.04–0.16), concentrated on alpha-tested leaves
and fine texture detail. Engines keep references per platform or GPU for this reason
(Unreal's screenshot tests store them per RHI and platform; Skia Gold per configuration).

## GPU time gates

- The render graph already times every pass with GPU timestamps (ADR 0011).
- On a machine that also does other work, interference only adds time: the fastest of a
  few runs' medians is a robust estimate. Medians over runs were measured to shift a whole
  scene by 9 % once; the fastest stayed within ±0.2 %.
- Small passes (a few µs) are dominated by noise: a regression must also exceed an
  absolute and frame-relative floor (5 µs, 0.5 % of the frame).
- CLAUDE.md's 3 % threshold applies; the gate stays advisory until a dedicated machine
  exists (docs/perf/reference-hw.md).

## Open questions

- More shots as features land: AO, TAA/TAAU (a converged sequence), SSS, upscalers, a
  streaming scene mid-way.
- Storing golden images outside the repository (Git LFS) if re-renders become frequent.
- HDR outputs (scRGB, HDR10): HDR-FLIP exists (Eurographics 2021) and could compare them.
