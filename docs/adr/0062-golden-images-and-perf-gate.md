# 0062. Golden images compared through FLIP, and a per-pass GPU time gate

- Status: Accepted
- Date: 2026-10-01
- Research: [docs/research/golden-images.md](../research/golden-images.md)
- Related: [ADR 0037](0037-checks-on-the-reference-machine.md) (checks on ref-1),
  [ADR 0011](0011-render-graph.md) (per-pass GPU timing)

## Context

Several changes to the renderer are now in flight at once (AO, SSS, FSR, diagnostics),
written in parallel. The render tests check a few pixels each (the lit side of a cube is
brighter than its shadow); nothing notices when a picture changes elsewhere, and nothing
notices a pass that got slower. CLAUDE.md §15 asks for golden-image render tests with a
tolerance on the reference GPU, and §5 for a performance regression threshold (3 %). A core
audit (2026-10-01) put this second, after crash diagnostics: it protects everything that
follows.

Questions:

- **how to compare images:** exact bytes break on any driver update; per-pixel thresholds
  (PSNR, max difference) do not know what the eye sees;
- **which references:** one per scene, or one per GPU vendor;
- **what the gate measures, and how it copes with a machine that is not dedicated to
  benchmarks** (docs/perf/reference-hw.md).

## Options

### Comparing images

1. **LDR-FLIP** (NVIDIA, HPG 2020; BSD-3) (chosen): the difference a viewer sees when
   flipping between the images, per pixel in 0..1, from contrast sensitivity at a viewing
   distance, colour distance in a perceptual space and edges/points present in one image
   only. Made for comparing renders; its mean and percentiles are thresholds that mean
   something.
2. **Exact match** (hashes): renders here are deterministic on one GPU and driver
   (measured: FLIP 0 between runs), but a driver update or another vendor changes bits
   everywhere.
3. **PSNR / RMSE / max absolute difference:** blind to perception: a 1 % exposure change
   everywhere and a missing shadow on a few pixels get similar numbers.
4. **SSIM:** structural, but grey-level oriented and less calibrated for colour and
   alternating comparison than FLIP.

### References

1. **One golden image per scene and vendor** (chosen): `tests/golden/images/<vendor>/`.
   Measured between the RTX and the Radeon (below): Sponza differs by a mean FLIP of
   0.013, above what a change should be allowed to be.
2. **One per scene with a tolerance wide enough for both vendors:** would hide changes of
   the size the tolerance allows.

### The gate

1. **Per-pass GPU time from the render graph's timestamps, per vendor baseline, the
   fastest of 3 runs' medians, 3 % and max(5 µs, 0.5 % of the frame), advisory by default**
   (chosen).
2. **Frame time only:** hides which pass moved and lets a slower pass hide behind a faster
   one.
3. **Criterion-style statistics over many runs:** slower, and the noise here comes from
   interference that only adds time, which "fastest of the runs" already removes.

## Numbers

ref-1, `engine_golden`, 640 × 360 golden images (1920 × 1080 for the gate), release.

- FLIP port against NVIDIA's reference: mean over its test pair 0.159693 vs 0.159691
  published (float summation order), 1920 × 1080 in 91 ms on 16 threads.
- Repeat renders on one GPU: FLIP 0 on all four scenes, both GPUs.
- Sensitivity (procedural scene / Sponza / flight helmet), mean FLIP (99.9th percentile):
  exposure +3 %: 0.060 (0.098) / 0.041 (0.064) / 0.055 (0.082); the sun turned by about
  1°: 0.019 (0.27) / 0.007 (0.31) / 0.002 (0.12); one glass cube's roughness 0.05 → 0.15:
  0.0014 (0.28). All beyond the tolerance (mean 0.01, 99.9th percentile 0.25).
- RTX vs Radeon: procedural 0.0046 (0.050), without sky 0.0020 (0.039), Sponza 0.013
  (0.150), flight helmet 0.0069 (0.155); the differences sit on alpha-tested leaves and fine
  texture detail (filtering, BC7 decoding).
- Golden images: 48–359 KB each, 1.1 MB for both vendors.
- The gate's noise, repeated runs: frame totals within ±0.2 % (RTX) and ±0.5 % (Radeon).
  With medians instead of the fastest run, one run on a busy machine read +9 % on every
  pass of a scene at once (clocks), hence the fastest of the runs. Inside
  `tools/check.ps1` while other builds ran, even the fastest run read +6.6 % on every pass
  of the procedural scene (idle machine: −0.1 %), hence the uniform-shift rule below.
- Baselines: RTX procedural 451 µs, Sponza 708 µs; Radeon 12.8 ms and 16.5 ms per frame.
- Time: the four golden tests take about 2 s per GPU with validation; the gate about 3 s
  (RTX) and 25 s (Radeon).

## Decision

- **`tests/golden`** (crate `engine_golden`, the `/tests` of CLAUDE.md §2):
  - `flip`: a Rust port of LDR-FLIP's CPU path (same constants, filters and order),
    checked against NVIDIA's published result;
  - `scene`: a test bed rendering fixed shots through the whole frame (culling, shadows,
    forward, local lights, sky light, transparency, bloom, tone mapping) and reading back
    8-bit sRGB plus per-pass GPU times; scenes are procedural (no files) or glTF from
    `test-assets/`;
  - `golden`: per-vendor golden images, `ENGINE_GOLDEN_UPDATE=1` to write them,
    `ENGINE_REQUIRE_GOLDEN=1` to fail on a missing one, renders and error maps in
    `target/golden/`;
  - `tests/golden.rs`: four shots (procedural with and without sky light, Sponza, the
    flight helmet), under validation with synchronization validation;
  - `bin/flip`: compare any two PNGs; `bin/perf_gate`: the gate (`--update`, `--strict`).
- **Tolerance:** mean FLIP ≤ 0.01 and 99.9th percentile ≤ 0.25 against the same vendor's
  image.
- **Gate rule:** slower by more than 3 % and by more than max(5 µs, 0.5 % of the frame).
  When every pass of at least 5 % of the frame moved within 2 points of the frame's own
  change, the scene is reported as a uniform shift (clocks, other work on the GPU) and not
  counted, except with `--strict`.
- **`tools/check.ps1`:** the `gpu` step runs the golden tests on both GPUs and requires
  every golden image; `-Perf` runs the gate on both GPUs, reports slower passes, fails
  only on errors (until a dedicated machine exists).
- A change that alters the picture on purpose updates the golden images in the same pull
  request, after looking at the new ones; the PR says why they changed.

## Consequences

- Visual regressions in covered features fail the checks with an error map to look at;
  intended changes cost a re-render and a look.
- Features not in the shots (AO, TAA, SSS, upscalers, streaming) are not covered until a
  shot turns them on: each such change adds its shot.
- Golden images grow the repository by about 1 MB per vendor per re-render; to revisit if
  it becomes a problem (Git LFS or a separate store).
- The gate is advisory on ref-1; `--strict` in CI when a dedicated machine exists.
- The test bed repeats part of `scene_bench`: `scene_bench` should move onto it when the
  parallel work touching it has landed.
