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

## Addendum (2026-10-10): rounds, measuring again, a short wait

The gate has grown to 15 scenes. In the check of 2026-10-10 it took 749 of the check's
901 s: 289 + 152 s waiting for 120 quiet CPU samples before each GPU (on the guess that
the APU stays hot after a build), then 19 s measuring the RTX and 279 s the Radeon. Even
so, the Radeon reported 5–28 % slower passes in a few scenes (shadows +27 % in two of
them), while a run on a calm machine is within ±1.4 % of its baseline on every scene.
Measured on ref-1 the same day:

- `perf_gate` itself keeps the CPU at 7–8 % on average (RTX 19 s, Radeon 279 s with 200
  frames): the scenes' loading is no load.
- Radeon, Sponza, frame total: idle 22.85–23.05 ms; while all 16 CPU threads spin,
  25.09–25.17 ms (+10 %); 4 s after they stop, 22.92 ms, and 22.84–23.00 ms for the next
  80 s. There is no cool-down to wait for: what slows the iGPU is work on the CPU *during*
  its runs (the die, power and memory are shared; `docs/perf/reference-hw.md`).
- A scene's three runs went one after another, about 15 s on the Radeon: a few seconds of
  other work slowed all three, and the fastest of them could not hide it.
- 100 timed frames instead of 200, every scene on the Radeon: frame totals −1.8 … +1.4 %
  against the 200-frame baseline (as the 200-frame run: −0.6 … +1.4 %), most passes
  within 0.1 %; 149 s instead of 279. The passes that move more (bloom in `sponza_ao`, the
  culling passes beside the atmosphere) overlap async compute and move as much with 200
  frames: their timestamps include the other queue's work (the review's P0 item on pass
  times, `docs/research/core-audit-2.md`).
- RTX: the two scenes with async compute (`sponza_ao`, `sponza_atmosphere`) read 26–29 µs
  longer end to end (`(gpu frame)`, +2.4 % and +3.6 %) than on 2026-10-02 with every pass
  unchanged, the same with a `perf_gate` built before the day's changes. The renderer did
  not change in between (one statistics function added), the machine did (AMD driver
  26.8.1, the SoC voltage, the iGPU's clock): the time went between the passes, where the
  queues hand work to each other.

Decision:

- **Rounds:** the runs go a round at a time, the first run of every scene, then the
  second, then the third. A slow moment slows one run of a few scenes, and each scene's
  fastest run stays clear of it.
- **Measured again:** a scene with a pass over the threshold gets `--runs` more runs (a
  round at a time again) and is judged by those alone, so that more runs cannot lower its
  time below what its baseline's runs could reach. A slow moment has passed by then, a
  regression has not; the first runs' numbers stay in the output.
- **A short wait:** `tools/check.ps1 -Perf` waits for 10 calm CPU samples (about 12 s,
  each under 20 %) before each GPU, long enough for a build or a test step to have ended,
  instead of 120.
- **100 timed frames per run** (`--frames N` to change it).
- **A baseline takes each pass's median run** (`--update`), a check its fastest run. The
  first check of this procedure, against baselines made of the fastest runs, measured
  `sponza_atmosphere`'s bloom downsample again and still read +23 %: its baseline,
  1,184.8 µs, came from one run, against 1,436–1,466 µs in every other run that day (the
  pass overlaps async compute). One lucky run must not set the bar.
- Both baselines re-measured this way; the files' header says how they were measured.

The same check showed the Radeon's other kind of noise: in its first runs the shadow pass
read 20–27 % slower in four scenes, in all three rounds (so for minutes, not seconds, with
the CPU quiet at 1 %), and normal when those scenes were measured again. A bandwidth-heavy
pass on the iGPU has a slow state that comes and goes; measuring again absorbs it, finding
its cause is left to the variance study in `docs/status.md`.

Consequences: the perf step took 237 s instead of 749 in the first check, four scenes
measured again included, and the whole check `-Gpu -Perf` 400 s instead of 901 (numbers in
`docs/perf/history.md`); a scene measured again costs three more runs (about 1 s on the
RTX, 10 s on the Radeon); a regression has to show in both sets of runs. The work on the
CPU during the Radeon's runs still counts: a build started meanwhile slows them, and the
uniform-shift rule names it.

Later the same day the pass times became each pass's own time on its queue, without the
previous pass's tail (ADR 0011 addendum); both baselines were re-measured with them.

## Addendum (2026-10-10, owner's decision): the Radeon's threshold

After the rounds and measuring again, the Radeon still reported most scenes slower in
some checks, with the CPU at 0 % and the RTX idle: Sponza's frame read 22.1–23.7 ms over
the day, and its memory-bound passes (shadows, bloom) 15–37 % slower for minutes at a
time, then normal again. It is the machine, not the code: a `perf_gate` built before the
day's changes read the same. A BIOS experiment (Data Fabric C-states, the memory at
JEDEC speed) could remove the cause; the owner chose to raise the iGPU's threshold.

- **The threshold depends on the vendor:** 3 % of the pass and 0.5 % of the frame on
  NVIDIA (its frames repeat within ±1 %); 10 % of the pass and 2 % of the frame on AMD.
  In the slow state the bloom passes read 10–37 % slower, which is 200–240 µs, about
  1 % of the frame. `--threshold` still overrides the percentage; the output names both.
- **A scene whose GPU ran slower as a whole is not counted** (without `--strict`): besides
  the uniform shift, when its frame total is slower by more than half the threshold and
  at least two thirds of its large passes (≥ 5 % of the frame, at least three) are too.
  In the Radeon's slow state forward, shadows and bloom slow together; a code regression
  mostly moves one pass (a forward pass 12 % slower moves the frame by ~9 % and no other
  large pass). Such scenes are not measured again: the state lasts minutes.
- **A busy GPU is reported before measuring:** `tools/check.ps1` samples each adapter's 3D
  engine for 3 s before each GPU's run and names the programs when one is over 10 %. The
  desktop redrawing animated windows took up to 30 % of the RTX's time (`dwm.exe`), and
  its async-compute scenes then read 14–41 % slower; with the windows minimized, nothing.

Consequences: on the Radeon only regressions of about 10 % or more are reported, which
still covers the iGPU-specific ones the gate exists for (the 64 → 72 VGPR forward shader
was 9–13 % slower); smaller ones show on the RTX unless they are AMD-only.

## Addendum (2026-10-15): the register check (queue item 6c)

The time gate cannot see a few registers more. The Radeon's environment shading went from
64 to 70 VGPRs under it (ADR 0093: the forward pass +5 % on Intel's Sponza, under the AMD
threshold), and a trial of the sun's PCF as 4 gathers raised 64 to 78 and cost 12 scenes
4–12 %. Registers do not vary from run to run as times do. The owner chose a register
check, as a warning (2026-10-15).

- **Recorded at creation.** On a device created with `ENGINE_PIPELINE_STATS=1`, every
  pipeline records the driver's statistics as it is made
  (`engine_rhi::take_pipeline_records`), with what identifies it:
  - its stages' entry points and SPIR-V hashes;
  - its specialization constants;
  - for graphics, what it draws to (formats, samples, alpha to coverage), which the
    compiled code can depend on.
- **Names that survive shader edits.** `perf_gate` hashes the repository's `.spv` files
  and names each stage by its file, so a pipeline reads
  `forward.vertexMain+forward.fragmentMain spec=0,1 R16G16B16A16_SFLOAT/D32_SFLOAT`.
- **Watched:**
  - vector registers (AMD `numUsedVgprs`, NVIDIA `Register Count`);
  - spills (AMD `scratchMemUsageInBytes`).
- **Not watched:**
  - AMD's scalar registers: on RDNA they never bound occupancy;
  - shared memory: the shader declares it;
  - NVIDIA's `Local Memory Size`: it reads 2^36 for every stage.
- **`perf_gate --registers`** renders every perf scene once and compares each pipeline
  with `tests/golden/perf/<vendor>.registers.tsv`.
  - One that uses more is listed with `**more registers**`. Fewer, new and gone ones are
    listed too.
  - Never fatal.
  - `--update` writes the baseline. A change that moves registers on purpose updates it on
    both GPUs and says why.
- **`tools/check.ps1 -Perf`** runs it after each GPU's time gate and shows the warnings in
  its summary.
- **Measured (ref-1):**
  - 191 pipelines per vendor: 14 s on the RTX, 33 s on the Radeon;
  - a second run reads every value the same;
  - a baseline lowered by hand (64 → 58 VGPRs) is reported.
- **Not covered:** the pipelines the vendor SDKs make themselves (FidelityFX, Streamline),
  and those no perf scene makes.
