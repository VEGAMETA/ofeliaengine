# Post-processing before tone mapping: bloom and automatic exposure

Checked 2026-09-30. Sources at the end. No code was copied.

## Why

The frame is HDR, and the tone mapper applied a fixed exposure given by the game. Two
things every engine adds before the tone mapper:

- **automatic exposure,** so a dark interior and a sunny street both land in the display's
  range, adapting over time like an eye or a camera;
- **bloom:** very bright light scatters in lenses and eyes, and without it HDR highlights
  just clip to white.

## How others do it

- **Bloom, Jimenez, "Next Generation Post Processing in Call of Duty: Advanced Warfare"**
  (SIGGRAPH 2014):
  - a chain of downsamples with a 13-tap filter (five overlapping 2 × 2 boxes: the center
    one weighted 1/2, the four corners 1/8 each), which avoids the pulsating artifacts of
    plain box filters;
  - Karis' average on the first step (each box weighted by 1 / (1 + luma)) against
    fireflies;
  - upsamples with a 3 × 3 tent, each level adding the enlarged smaller levels.
- **Unreal and Bevy** (MIT/Apache, `bevy_post_process::bloom`): the same chain.
  - Bevy composites "energy-conserving": the bloom replaces a fraction of the image
    instead of being added, so a uniform image stays unchanged and brightness is not
    created.
  - Bevy also offers a threshold ("prefilter") for a non-physical look.
- **Automatic exposure, histogram-based (Unreal's "eye adaptation", Bevy's
  `AutoExposure`):**
  - a histogram of log luminance over the frame (or a downsampled frame);
  - the mean of the pixels between two percentiles, so a few very dark or very bright
    pixels (the sun, black bars) do not drive it;
  - an exposure that maps that mean to middle grey (0.18 × 2^compensation);
  - adaptation over time, faster towards bright than towards dark;
  - everything on the GPU, the result read by the tone mapper, no readback.
  - Bevy's default filter is 10%–90%.
- **Godot** (MIT): average luminance by a mip chain reduction, then adaptation. Cheaper,
  but a few bright pixels move the mean.
- **Lagarde and de Rousiers, "Moving Frostbite to PBR"** (2014): exposure in EV100 from
  physical camera settings. A later option for artists who want manual cameras.

## Conclusions

1. **Bloom:**
   - Jimenez's chain, up to 6 levels from half resolution;
   - Karis' average on the first step (a setting, on by default);
   - upsample level k = lerp(own downsample, tent of level k + 1, scatter), so every level
     stays normalized;
   - the tone mapper mixes it in by `intensity` (energy-conserving).
2. **Automatic exposure:**
   - a 256-bin histogram of log2 luminance (−12..12) from every second pixel along each
     axis, counted in groupshared memory, then added once per bin and group;
   - the 10%–90% mean, then the exposure towards 0.18;
   - EV adaptation of 1 − exp(−dt · speed) per frame (1.5 EV/s up, 3 EV/s down);
   - the tone mapper multiplies by it.
3. **Tests:**
   - a uniform image blooms into itself;
   - a bright block keeps its energy and stays symmetric;
   - a uniform scene reaches 0.18;
   - 5% blinding pixels do not move the exposure;
   - one adaptation step matches the formula.

## Sources

- Jimenez, "Next Generation Post Processing in Call of Duty: Advanced Warfare", SIGGRAPH 2014 Advances in Real-Time Rendering (https://advances.realtimerendering.com/s2014/)
- Karis, "Tone Mapping" (the 1 / (1 + luma) average), graphicrants.blogspot.com, 2013
- Bevy, `crates/bevy_post_process/src/bloom` and `auto_exposure`, main, checked 2026-09-30 (MIT OR Apache-2.0)
- Unreal Engine documentation, "Auto Exposure" (histogram, percentiles), checked 2026-09-30
- Lagarde, de Rousiers, "Moving Frostbite to Physically Based Rendering", SIGGRAPH 2014 course notes
