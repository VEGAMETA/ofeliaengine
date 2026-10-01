# 0059. Subsurface scattering: a sum-of-Gaussians profile, a forward subsurface pass and a disk-sampled scatter pass

- Status: Accepted
- Date: 2026-10-01
- Research: [docs/research/subsurface-scattering.md](../research/subsurface-scattering.md)
- Related: [ADR 0040](0040-materials-and-textures.md) (materials),
  [ADR 0042](0042-depth-prepass.md) (depth prepass),
  [ADR 0044](0044-cascaded-shadow-maps.md) (shadow maps),
  [ADR 0055](0055-pipeline-cache-and-warm-up.md) (pipelines),
  ADR 0058 (GTAO, in flight: also forces the prepass)

## Context

Skin, wax, marble, jade, leaves and fruit look like plastic with a Lambertian diffuse
term: light that enters them comes out around where it entered, softening shading and
shadow edges, reddening skin at the terminator, and passing through thin parts. SSS is
item 4 of the team's priorities (2026-09-30), with ambient occlusion.

To decide: the profile (the shape of the spread), the technique and its passes, how the
material describes it, transmission, and what the iGPU of ref-1 can afford.

## Options

### Profile

1. **Sum of Gaussians** (d'Eon and Luebke 2007; chosen): per channel, weights of up to
   six Gaussians. Everything is analytic: reflectance at a distance, energy within a
   radius (CDF), slab transmission `Σ wᵢ exp(−d²/2vᵢ)`, half-plane response
   `Σ wᵢ Φ(x/√vᵢ)`. A measured skin fit exists (2007, prior art). Six Gaussians cost six
   exponentials per evaluation: the scatter pass precomputes them per sample.
2. **Burley's normalized diffusion** (2015; HDRP, O3DE, Golubev 2018): two exponentials
   per channel, one intuitive parameter, closed-form CDF. **Covered by Disney's US patent
   9,659,404 B2** ("Active, anticipated expiration 2035-03-24", claim 1 broad): not used
   while the humans decide.
3. **Measured or tabulated profiles:** any shape, but tables to sample on the GPU and no
   closed forms for tests.

The profile sits behind one Rust type, `SubsurfaceProfile` (evaluate, CDF, sample
radius, transmission, half-plane response), and the shader functions that read its GPU
form (`profile_transmission`, `profile_cdf`, the kernel): swapping the shape changes those
and the kernel builder, nothing else.

### Technique

1. **Disk-sampled screen-space scatter** (Golubev 2018; chosen): one pass, any radial
   profile, samples importance-sampled from the profile.
2. **Separable screen-space blur** (Jimenez 2015): two passes of ~17 samples, cheaper at
   large radii; exact only for a Gaussian, cross-shaped artifacts otherwise.
3. **Pre-integrated LUT in the forward pass** (Penner 2011): nearly free; wraps light
   around curvature but spreads nothing across shadow edges or normal-map detail. Kept as
   the planned low tier (see "Not done").
4. **Texture-space diffusion:** per-object render targets and seams; obsolete in games.

### Where the diffuse light comes apart

1. **A forward pass of its own for subsurface batches** (chosen): "forward subsurface"
   after "forward", depth tested for equality (the prepass runs), writing color plus two
   targets. Ordinary pipelines keep one attachment; dynamic rendering needs equal
   attachment counts within a pass.
2. **Extra targets on every forward pipeline:** doubles the forward pipelines and writes
   12 more bytes per pixel for every material.

### How the scatter pass reaches only subsurface pixels

1. **Full-screen triangle discarding elsewhere** (first version): 0.64 ms of fixed cost on
   the Radeon iGPU at 1080p (one texel read per pixel), ~30 µs on the RTX.
2. **Compute pass over tiles:** the color targets views own have no STORAGE usage
   (`viewport.rs`), so it would write a separate texture and need a composite.
3. **The subsurface batches drawn again** (chosen): `depthVertex` (bit-identical
   positions) with the scatter fragment shader, depth tested for equality, additive
   blending into the color target. Cost follows the subsurface pixels; the vertices are
   transformed a third time.
4. **Stencil:** the engine's depth targets have no stencil aspect.

## Measurements

`subsurface_bench` (new example): a skin-profile panel covering 10/50/100% of a
1920×1080 HDR target in front of an ordinary wall, sun + ambient, thick transmission on.
Profile at its real size seen from 1 m (99% of the light within 4 px, "×1") and four
times larger (15 px, "×4"). Release build, medians of 120 frames, commit of this ADR.

Scatter pass, µs (8 / 20 / 40 / 80 samples):

| coverage | RTX 4060 Ti ×1 | RTX ×4 | Radeon iGPU ×1 | Radeon ×4 |
|---|---|---|---|---|
| 10% | 17 / 27 / 43 / 70 | 18 / 33 / 59 / 111 | 396 / 410 / 422 / 435 | 529 / 964 / 1,686 / 3,040 |
| 50% | 57 / 76 / 115 / 188 | 72 / 134 / 243 / 456 | 1,945 / 2,018 / 2,075 / 2,168 | 2,678 / 4,802 / 8,534 / 15,353 |
| 100% | 108 / 111 / 114 / 117 | 137 / 256 / 465 / 884 | 3,902 / 3,970 / 4,167 / 4,295 | 5,286 / 9,625 / 16,826 / 30,146 |

(×1 hardly depends on the budget: its kernel covers so few pixels that the per-pixel
budget rule drops to 8 samples.)

Other costs at 100% coverage: the forward subsurface pass 145 µs against 137 µs for
ordinary shading of the same pixels (RTX; Radeon 3,604 against 3,218, +12%: two more
targets and the transmission lookup); the forced prepass 15 µs (RTX), 196–317 µs
(Radeon).

How it got there (RTX, 100%, ×4, 80 samples / Radeon, 10%, 8 samples):

| version | RTX µs | Radeon µs |
|---|---:|---:|
| full screen, Gaussians read per sample, sin/cos per sample | 2,244 | 864 |
| Gaussians in registers, spiral precomputed | 988 | — |
| batches drawn again (no full-screen cost), weights precomputed per sample, depth falloff per channel, per-pixel budgets | 884 | 529 |

Ordinary scenes are unchanged: Sponza (`scene_bench`, textured) forward pass 318 µs
before and after (RTX), 7,920–8,120 before and 7,981–8,126 after (Radeon); the opaque
fragment shader keeps 56 registers (NVIDIA) and 48 VGPRs (AMD). The subsurface
pipelines (9) are created with the first profiles: cold 89–102 ms (RTX) and 91–94 ms
(Radeon) in parallel, 14 / 1 ms warm; the renderer's own 41 pipelines are unchanged.

Correctness (GPU tests, both GPUs, validation with synchronization validation clean):
even light unchanged at every budget within 0.24% (8-bit albedo); zero radius equals
ordinary shading within two half-float ulps plus 0.3%; a shadow edge's response within
0.86% of the lit value of `SubsurfaceProfile::half_plane`; no light across a 1-unit depth
step or from ordinary neighbours (0 exactly); thin transmission within 0.1% and thick
within 0.25% of `SubsurfaceProfile::transmission`.

## Decision

- **Profile:** `SubsurfaceProfile`, a sum of up to 6 Gaussians, weights normalized per
  channel (it only moves light), in profile units times a scale in world units.
  Presets: `skin()` (d'Eon and Luebke's six Gaussians, mm, scale 0.001) and
  `from_radius([r, g, b])` (one Gaussian per channel, 99% within the radius). The
  renderer holds up to 255 (`set_subsurface_profiles`), uploaded through the ring each
  frame that draws subsurface materials (2,496 bytes each).
- **Material:** `MaterialDesc::subsurface: Option<Subsurface>`: profile index, mask (how
  much of the diffuse light scatters), transmission (none / thin / thick), texturing
  (post-scatter, or pre- and post-scatter with the square root of the base color on each
  side), thickness and thickness texture (G). The material keeps its 80 bytes: the three
  spare words hold the packed bits, the thickness texture and the thickness.
  `MaterialKind::subsurface` puts it in a pipeline state of its own (bit 4: 32 states);
  blended materials ignore it.
- **Passes** (when a view draws subsurface batches and there are profiles): the depth
  prepass is forced (OR'ed with the depth-prepass setting, as GTAO does); "forward"
  draws the other opaque states; "forward subsurface" draws the subsurface states with
  the equal-depth test and writes color (specular, emission, and the unscattered
  `1 − mask` part), `subsurface irradiance` (RGBA16F: mask × diffuse light per unit
  albedo × entry albedo, and the depth from the near plane; cleared to 0) and
  `subsurface albedo` (RGBA8 sRGB: exit albedo, profile index); "subsurface scatter"
  draws the subsurface batches again and adds `albedo × scattered light`. Then sky,
  transparent pass, TAAU as before.
- **Scatter:** per pixel, the center weighted by the profile's CDF at half a pixel;
  samples on a golden-angle spiral with radii stratified in the channel-averaged CDF
  (precomputed per profile for budgets 8/20/40/80, with the profile over the density per
  channel), turned per pixel by interleaved gradient noise (frame index only under a
  temporal view, the rule GTAO uses); depth falloff `exp(−dz²/2v̄)` with each channel's
  mean variance (exact for one Gaussian per channel, an approximation for mixtures);
  other materials skipped; per-channel renormalization; no samples when the kernel lies
  within half a pixel; a smaller budget where it already has one sample per 4 px² of the
  disk. `SubsurfaceQuality::Medium` (20) by default.
- **Transmission** (sun only), per unit albedo `(1 − metallic)/π · light · T(d) ·
  max(−n·l, 0)`: thin uses the material's thickness and the shadow test of the receiver
  pushed through that thickness; thick uses `max(thickness, shadow-map distance)`.
  Single-sided subsurface materials cast shadows from their faces towards the light (the
  shadow map then holds where light enters; others keep casting from back faces).
- **Pipelines:** 9 (per shading variant, the subsurface pass without and with motion
  vectors, and one scatter pipeline), created by the first `set_subsurface_profiles`,
  through the pipeline cache. The permutation axes are the existing ones (local lights,
  environment, motion vectors); nothing is added for the transmission mode or profile
  (data, not code).
- **glTF:** `KHR_materials_volume`'s `thicknessFactor` and `thicknessTexture` are
  imported into `MaterialData`. No ratified extension describes a scattering profile
  (`KHR_materials_subsurface` is an initial draft), so profiles come through the API.

## Consequences

- Subsurface materials cost a pass of their own, the prepass and the scatter pass; other
  materials cost nothing (same registers, same times).
- The exit albedo is 8-bit sRGB, like a G-buffer: both GPUs store it 0.1–0.2% off.
- The forward shader's lighting functions accumulate the diffuse irradiance apart
  (`inout` parameters, compiled away in ordinary pipelines); GTAO's change to
  `ambient_light` meets `ambient_diffuse` here at merge time: the screen-space occlusion
  must multiply the subsurface diffuse ambient too.
- Screen-space limits: nothing scatters from off-screen or hidden surfaces; light
  crosses between separate objects of the same depth when both scatter (the bilateral
  weight only sees depth); the disk lies in the screen plane, so grazing surfaces
  scatter less along their slope.

## Not done

- **The pre-integrated LUT tier.** Measured first, as planned: 8 samples on the Radeon
  iGPU cost ~1.9–2.6 ns per subsurface pixel, 1.2–1.6× the forward shading of that pixel
  (a close-up face at 20% of 1080p: ~0.8 ms). It is the team's call (below) whether that
  is too slow for the iGPU default; the LUT (64×64 per profile from the same profile,
  curvature from derivatives) is the next step if so.
- Transmission from local lights; subsurface in the transparent pass; profiles in cooked
  packs (the thickness is imported from glTF, not cooked yet: a pack format change).

## Decided since (2026-10-01)

- **Patent:** the team does not touch Burley's normalized diffusion (US 9,659,404 B2): the
  sum of Gaussians is the engine's profile, not a placeholder.
- **iGPU tier:** the screen-space scatter stays the default everywhere; the Radeon iGPU
  is a correctness machine (docs/perf/reference-hw.md) and ~0.4 ms per 10 % of 1080p at 8
  samples is acceptable there. The LUT tier waits for a weak target GPU.
- **Golden image:** `procedural_sss` (tests/golden): the floor and the dielectric spheres
  with a 0.3/0.12/0.06-unit profile; mean FLIP 0.014 against the scene without scattering,
  on the shadows' edges.
