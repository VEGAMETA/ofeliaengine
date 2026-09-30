# 0047. Bloom and automatic exposure

- Status: Accepted
- Date: 2026-09-30
- Builds on: [ADR 0038](0038-hdr-output.md) (the tone mapper and HDR output),
  [ADR 0011](0011-render-graph.md) (the graph, fixed here)
- Research: [`docs/research/post-processing.md`](../research/post-processing.md)

## Context

The frame is HDR, and the tone mapper applied the exposure the game gave it:
- scenes of different brightness needed hand-tuned exposure;
- highlights clipped without the glow that makes them read as bright.

CLAUDE.md §6 lists post-processing; this is its first part. The tone mapper itself is next.

## Options

**Exposure:**

1. A fixed or scripted exposure: what existed.
2. The average luminance by mip reduction (Godot): cheap, but the sun or a black border
   moves it.
3. **A histogram between percentiles (Unreal, Bevy) (chosen):** robust against small
   extremes, still one frame and entirely on the GPU.

**Bloom:**

1. A threshold and a Gaussian blur: cheap and non-physical, and it creates light.
2. **Jimenez's downsample / tent-upsample chain, mixed in energy-conserving (chosen):**
   the reference in games; normalized filters.

## Decision

- **`engine_render::postfx::Bloom`:**
  - up to 6 levels from half resolution (RGBA16F transient textures): "bloom downsample"
    with the 13-tap filter, Karis' average on the first step
    (`BloomSettings::karis_average`, default on), then "bloom upsample" with a 3 × 3
    tent;
  - level k = lerp(own downsample, tent of level k + 1, `scatter` = 0.7);
  - `add_passes` returns a `BloomOutput` (the half-size bloom and `intensity`, 0.04).
- **`engine_render::postfx::AutoExposure`,** one per view:
  - "exposure histogram clear", "exposure histogram" (256 bins of log2 luminance in
    −12..12, from every second pixel along each axis, groupshared counts then one atomic
    per bin and group), "exposure average" (one group: prefix sums, the 10%–90% mean,
    the exposure towards 0.18 × 2^compensation);
  - adaptation of 1 − exp(−dt · speed) of the EV difference per frame, 1.5 EV/s towards
    brighter exposure and 3 EV/s towards darker; `reset()` after cuts;
  - the exposure stays in a 16-byte GPU buffer.
- **`Composite::bloom` and `Composite::auto_exposure`:**
  - the tone map pass mixes the bloom in (`lerp(image, bloom, intensity)`) and multiplies
    by the measured exposure;
  - `Composite::exposure` then acts as compensation;
  - without them nothing changes.
- **Render graph fix:**
  - an access that reads and writes (`StorageReadWrite`) counted its own writes as
    visible, so a later read by the same stages got no barrier;
  - found here: the exposure average read a histogram still being counted, only on the
    RTX, invisible to the synchronization validation (device-address accesses);
  - now any write clears the visibility; the unit test
    `a_read_after_a_read_write_access_waits_for_its_writes` fails without the fix.
  - The engine's other read-write accesses are read by other stages (indirect arguments,
    fragment shaders), which already got barriers.
- **`scene_bench --post`** measures bloom, exposure and the tone map pass.

## Measurements

ref-1, release, Sponza at 1920 × 1080, GPU µs (medians):

| | RTX 4060 Ti | Radeon iGPU |
|---|---:|---:|
| bloom (6 levels) | 114 | 1,114 |
| automatic exposure | 17 | 280 |
| tone map pass (with bloom and exposure) | 23 | 821 |

**Steps on the way,** exposure:

| | RTX | Radeon |
|---|---:|---:|
| every pixel, a single-thread average | 37 | 483 |
| every second pixel along each axis | 27 | 295 |
| a 256-thread parallel average | 17 | 280 |

**Correctness** (`tests/post.rs`, both GPUs, synchronization validation):

- a uniform image blooms into itself: worst relative error 0.05%;
- a 2 × 2 block at 1000:
  - keeps 98.6% of its energy without Karis' average (the rest leaves through the
    borders' clamping);
  - keeps 61% with it: small bright sources give up energy by design;
  - symmetric in both cases;
- uniform luminance 4 → exposure 0.0440 (0.18 / 4 = 0.045; histogram bins are 0.09 EV
  wide);
- 5% of the pixels at 1000 leave the exposure unchanged;
- 0.1 s after a jump from luminance 4 to 1: −4.229 EV on the RTX and −4.216 on the
  Radeon, for −4.223 expected.

## Consequences

- **Scenes get exposure and glow without per-scene tuning.** The tone mapper (AgX, with
  HDR output) is the next piece.
- **Not done yet:**
  - bloom thresholds and lens dirt;
  - physical camera exposure (EV100);
  - metering masks (center-weighted).
- **Next, from measurements:**
  - bloom is the largest cost, 1.1 ms on the iGPU. Options: R11G11B10 levels (needs
    `shaderStorageImageExtendedFormats`), fewer levels on the iGPU, a single-pass
    downsampler.
