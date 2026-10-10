# 0083. HDRI environments: RGBE panoramas decoded on the GPU, a background cube, the sun as a directional light

- Status: Accepted (phase 1; phase 2, cooking, below)
- Date: 2026-10-11
- Research: [docs/research/hdri-environments.md](../research/hdri-environments.md)
- Related: [ADR 0046](0046-image-based-lighting.md) (the image-based lighting it feeds),
  [ADR 0069](0069-sky-atmosphere-and-fog.md) (the physical sky, which replaces it when on),
  [ADR 0073](0073-reflection-probes-and-screen-space-reflections.md) (probes capture its lighting cube), queue item 2b

## Context

The owner asked for HDRI environments before GI (2026-10-10): a panorama photographed or
rendered in HDR, behind the scene and lighting it. The environment knew a three-color
gradient and the physical atmosphere, both drawn into a 128² radiance cube that is the
background, the source of the prefiltered specular cube and of nine irradiance harmonics.

A real panorama breaks that in three ways (measured on the test panorama, Poly Haven's
Kloofendal 48d Partly Cloudy, 2K, CC0):

- its sun is a spike 10⁵ times the sky (luminance 72,815 against 0.7) and holds 68 % of
  the light on an upward surface: in the 128² cube it becomes fireflies in the prefiltered
  levels and ringing in the harmonics, and it casts no shadow;
- six of its channel values exceed half floats' 65,504;
- a 128² background is far blurrier than the panorama (2,048 pixels around).

## Options

1. **The sun:**
   - **taken out into a directional light** (chosen): found in the panorama, its light
     measured, its cone filled with the sky around it in the lighting cube only;
   - luminance compression (Filament's `cmgen --clamp`): the spike shrinks, but the
     sun's energy goes and it still casts no shadow;
   - more samples, blue noise, tone-mapped filtering (Godot, Bevy): less noise, no shadow.
2. **The background:**
   - **a cube of its own made from the panorama** (chosen): every reader of the sky (the
     sky pass, the atmosphere's composite under fog, reflection probes) stays a cube read;
   - the panorama sampled directly by the sky pass (Godot): no conversion, but each reader
     needs a panorama variant.
3. **Texels on the GPU:**
   - **RGBE as the file holds it, decoded by the shaders** (chosen): 4 bytes per pixel,
     nothing lost;
   - RGBA16F or RGB9E5 converted on the CPU: 8 or 4 bytes, the range cut at 65,504.
4. **Where the conversion runs:**
   - **on the GPU, in the first frame that draws the panorama** (chosen for phase 1):
     any `.hdr` loads at run time (user content, the editor, tests);
   - cooked offline into BC6H cubes with mips (phase 2: the DDC and the pack, ADR 0056).

## Measurements

Release build, the 2K test panorama, `panorama_bench` (each timed frame turns the panorama,
so everything is remade every frame; medians of 30 frames; 1920 × 1080 for the sky pass):

| | RTX 4060 Ti | Radeon iGPU (Raphael, 2 CUs) |
|---|---:|---:|
| read, decode, find the sun (CPU) | 1.0, 5.7, 4.6 ms | 0.9, 5.4, 4.6 ms |
| `Panorama::new` (texels written, cube, pipelines) | 13.2 ms | 11.2 ms |
| background cube 512² with mips (once) | 226 µs | 6.8 ms |
| lighting cube 128² from it (gradient: 13 / 46 µs) | 81 µs | 539 µs |
| prefilter, irradiance (unchanged) | 30, 10 µs | 682, 2 µs |
| sky pass per frame (gradient → panorama) | 36.9 → 37.9 µs | 710 → 721 µs |

Memory: the texels (8 MiB for 2K, 32 MiB for 4K) and the background cube with mips (16
MiB at 512², 64 MiB at 1,024²).

The sun search on the test panorama: 47.89° up (the name says 48°), illuminance (4.36,
4.39, 3.98), a cone of 1.05°; the independent measurement gave 4.38 within 1°. On the GPU
(`environment.rs` test, synchronization validation on): a panorama whose color is its
direction shows the right one along all six axes and turned; a white one is a white
furnace within 0.2 %; with the sun left out an upward face gets the sky's light alone
(0.2515 against 0.25), with it kept the sun's too (0.527 against 0.536), and the
background keeps the sun either way. Golden shot `procedural_hdri` on both GPUs, the same
from run to run.

## Decision

- `engine_assets::hdr`: our own decoder and encoder (flat and run-length encoded
  scanlines), RGBE kept; `find_sun`: the brightest pixel is a sun if it outshines the
  median of the sky 10° to 20° around it 256-fold; its cone ends a pixel past the first
  one-pixel ring whose mean falls below 1/1024 of the peak; its illuminance is the light
  within the cone above the mean sky just outside it.
- `engine_render::panorama::Panorama`: the texels in a buffer the GPU reads (device-local
  and host-visible where available, written once), a background cube made on the GPU
  (`panoramaCube`, 2 × 2 samples per texel; faces a quarter of the width, at most 1,024 by
  default), the sun (turned with the panorama).
- `Environment::set_panorama`: the radiance cube is filled from the background cube
  (`panoramaLight`, 4 × 4 samples per texel at the matching mip), the sun's cone filled
  with the sky around it; the rest of ADR 0046 is unchanged. The sky pass and the
  atmosphere's composite draw the background cube and no disk of the directional light
  (the panorama shows its own sun). Reflection probes capture the radiance cube, without
  the sun, like the shading (the light gives its highlight). The game lights the scene
  with a directional light from `Panorama::sun`.
- Turning (`Panorama::set_rotation`) remakes the background and the lighting.
- The physical atmosphere, when on, replaces the panorama, as it does the gradient.

## Consequences

- HDRI skies light scenes with shadows from their sun, no fireflies and no ringing, at no
  cost per frame.
- Setting or turning a panorama costs a few compute passes once (6.8 ms on the iGPU for a
  512² background); the texels stay in memory for turning.
- **Phase 2** (queue item 2b continues): cooking panoramas into BC6H cubes with mips
  (DDC, pack; the background 8 times smaller), OpenEXR input, the sun's angular size for
  soft shadows. A panorama without a sun in it (overcast) keeps all of its light in the
  lighting cube, as before.
