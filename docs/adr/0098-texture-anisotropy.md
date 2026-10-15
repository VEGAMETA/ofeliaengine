# 0098. Anisotropic filtering as a player setting

- Status: Accepted
- Date: 2026-10-15
- Related:
  - [ADR 0060](0060-fsr-3-1-upscaling.md): the texture LOD bias, held by the material samplers;
  - queue item 6d (the owner, 2026-10-15: a player's setting, 16× by default).

## Context

Material textures are filtered trilinearly with 16× anisotropy. Games offer the player
off, 2×, 4×, 8× and 16×: fewer samples cost less where textures are seen at a slant, and
blur them there.

Measured on 2026-10-15 (the materials' samplers changed by hand, the perf gate's scenes,
both GPUs), the forward pass against 16×:

| anisotropy | Radeon iGPU | RTX 4060 Ti |
|---|---|---|
| 4× | −1.5 to −2.5 % | −2.4 to −3 % |
| 2× | −7 % | −7 to −9 % |
| off (trilinear) | −15 to −19 % | −13 to −14 % |

The owner chose a player setting with 16× by default (2026-10-15).

## Options

1. **In the samplers, as the LOD bias (ADR 0060).** Material sampler sets for each pair of
   LOD bias and anisotropy, made on first request; the material table points its
   materials at one.
   - Nothing in the shaders.
   - A few samplers per set; changing the setting re-resolves the table's materials once.
2. **A shader parameter.** The anisotropy in the material data, applied through explicit
   gradients.
   - Every shading pipeline pays registers for it: a bias in the shader cost the Radeon
     8 VGPRs in each (ADR 0060).
3. **The default set's samplers rewritten in place.**
   - Frames in flight read those descriptors: rewriting them waits for the GPU.
   - Every material table of the device changes at once.

## Decision

**Option 1.**

- **`engine_render::material::Anisotropy`:** `Off`, `X2`, `X4`, `X8`, `X16` (the default).
  `Anisotropy::ALL` lists them for a settings menu.
- **`MaterialTable::set_texture_anisotropy(&bindless, anisotropy)`**, beside
  `set_texture_lod_bias`.
  - Every view drawing with the table's materials gets it.
  - Changing it re-resolves the table's materials in the next frame.
- **`Bindless::material_samplers(bias, anisotropy)`:**
  - The default set (no bias, 16×) is always there.
  - Up to 16 other sets, of any bias and anisotropy, are made on first request (8 before,
    for the bias alone). Beyond that, the nearest set comes back, one with the same
    anisotropy first.
  - Off is trilinear filtering without anisotropy.
  - The anisotropy is capped at the device's limit.

## Measurements

- **The default path is the same.** At 16× and no bias, the table uses the default set,
  as before. The perf gate's registers and times were unchanged.
- **The setting reaches the samplers:** `the_players_anisotropic_filtering_is_applied`.
  - The frame renderer under validation, on both GPUs, looks along a floor from 0.5 above
    it; its texture's levels are in solid colors.
  - Floor pixels from the finest two levels: 16× 7,936, off 6,672 (RTX) and 6,724
    (Radeon).
  - From the coarse ones: 16× 256 (RTX) and 100 (Radeon), off 1,280 on both.

## Consequences

- A game can offer the setting. 16× stays the default on every device class: on the iGPU
  4× would save 1.5–2.5 % of the forward pass, and the owner kept 16×.
- The player crate (queue item 8a) will hold it with the other player settings.
- Other samplers (the built-in kinds, post-processing) are unaffected.
