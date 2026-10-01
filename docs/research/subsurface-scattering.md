# Subsurface scattering

Research for screen-space subsurface scattering (item 4 of the team's priorities,
2026-09-30). Decision: [ADR 0059](../adr/0059-subsurface-scattering.md).

Checked on 2026-10-01. Papers and talks are described from their text; engines from
their public documentation and, where the license allows adapting code (O3DE, iryoku's
separable SSS, Godot, Filament), from their source. No code was copied: the engine's
shaders and profile code are written from the formulas below. Unity HDRP (Unity
Companion License), Unreal Engine and Blender (GPL) were not read as code.

## Sources

| Source | Version / date | License | What we take |
|---|---|---|---|
| E. d'Eon, D. Luebke, "Advanced Techniques for Realistic Real-Time Skin Rendering", GPU Gems 3, ch. 14 | 2007 | book (free online) | the sum-of-Gaussians diffusion profile and its six-Gaussian skin fit (variances, weights); transmission as the profile at a distance |
| J. Jimenez, V. Sundstedt, D. Gutierrez, "Screen-Space Perceptual Rendering of Human Skin", ACM TAP | 2009 | paper | screen-space scattering of the diffuse light only, depth-aware |
| J. Jimenez, D. Whelan, V. Sundstedt, D. Gutierrez, "Real-Time Realistic Skin Translucency", IEEE CG&A | 2010 | paper | transmission from the shadow map's distance through the object, `T(d)` from the profile |
| J. Jimenez et al., "Separable Subsurface Scattering", CGF; iryoku/separable-sss | 2015 | paper; code BSD-2 (read) | per-channel renormalization; the 1D separable kernel (not taken, see below) |
| E. Penner, "Pre-Integrated Skin Shading", SIGGRAPH course | 2011 | talk | the LUT tier (deferred) |
| E. Golubev, "Efficient Screen-Space Subsurface Scattering Using Burley's Normalized Diffusion in Real-Time", SIGGRAPH course | 2018 | talk | disk sampling with importance-sampled radii, golden-angle spiral rotated per pixel, profile at the 3D distance as the bilateral weight, the footprint shortcut; **not** its Burley profile |
| T. Xie, M. Olano, B. Karis, K. Narkowicz, "Real-time subsurface scattering with single pass variance-guided adaptive importance sampling", I3D | 2020 | paper | per-pixel sample budgets (in a simpler form) |
| M. Pettineo (MJP), "An Introduction To Real-Time Subsurface Scattering" | 2019 blog | blog | overview of the families |
| O3DE `ScreenSpaceSubsurfaceScatteringCS.azsl` | development branch | Apache-2.0 OR MIT (read) | a compute implementation of Golubev's scheme; its pass structure |
| J. Jimenez, "interleaved gradient noise" (Next Generation Post Processing in Call of Duty: Advanced Warfare, 2014) | 2014 | talk | the per-pixel rotation (already in the engine's shadow dithering) |
| glTF `KHR_materials_volume` (ratified), `KHR_materials_diffuse_transmission` (release candidate), `KHR_materials_subsurface` (initial draft) | registry, 2026-10-01 | spec | thickness import; no subsurface extension is ratified |
| US patent 9,659,404 B2 (Disney, normalized diffusion) | Google Patents status "Active, anticipated expiration 2035-03-24" | patent | a reason **not** to use Burley's profile (see ADR 0059) |

## The families

### Texture-space diffusion (d'Eon and Luebke 2007)

Irradiance rendered into the mesh's texture space, blurred by each Gaussian of the
profile, recombined. Exact on the surface; one texture per character, a pass per
Gaussian, overdraw and seams. Superseded by screen-space methods in games.

### Screen-space, separable (Jimenez 2009, 2015)

Two 1D blurs of the diffuse light in screen space, kernel scaled by depth, samples from
other depths lerped towards the center. Cheap (2 × ~17 samples). The separable kernel is
exact only for a Gaussian; for other profiles it is an approximation that shows as
cross-shaped artifacts. Needs a stencil or mask to skip other pixels.

### Screen-space, disk sampling (Golubev 2018, HDRP, O3DE)

One pass: per pixel, samples on a disk whose radii are drawn from the profile's CDF,
rotated per pixel (TAA averages the noise), each weighted by the profile at the 3D
distance over the sampling density, per-channel renormalization. Works for any radial
profile; cost ∝ samples. HDRP adds a pixel-footprint shortcut (a kernel under a pixel:
no samples) and per-pixel budgets. HDRP's and Golubev's profile is Burley's normalized
diffusion (see the patent note).

### Pre-integrated (Penner 2011)

A LUT (curvature × N·L) of the profile integrated over a ring: light wraps around the
terminator in the forward pass, no extra pass. Blurs nothing along flat lit areas
(shadow edges, normal maps are handled separately). The cheapest tier; good for mobile
and iGPUs.

### Transmission (Jimenez 2010)

Thin parts (ears, nostrils, leaves) let light through from behind. With a sum of
Gaussians, the profile integrated over a plane at distance `d` is `Σ wᵢ exp(−d²/2vᵢ)`
(derived in ADR 0059's tests). `d` is a thickness from the material, or the distance from
the shadow map's nearest surface to the receiver (the shadow map must hold the faces
towards the light).

## Profiles

- **Sum of Gaussians** (d'Eon & Luebke 2007): per channel, weights and variances.
  Analytic everything (evaluation, CDF, slab transmission, half-plane response), fitted
  to measured skin in 2007. Six Gaussians for skin: costlier to evaluate per sample than
  Burley's two exponentials, which is why the scatter pass precomputes it.
- **Burley normalized diffusion** (2015): `(e^{−r/d} + e^{−r/3d}) / (8πd r)`, one
  parameter per channel, closed-form CDF. Patented (US 9,659,404 B2, claim 1 broad,
  active until 2035 per Google Patents): not used pending the team's decision.
- **One Gaussian per channel**: a special case of the sum; exact depth falloff in the
  scatter pass. Enough for wax, jade, marble, fruit.

## What we take

- Disk sampling (Golubev) with a sum-of-Gaussians profile behind one CPU type
  (`SubsurfaceProfile`: evaluate, CDF, sample radius, transmission, half-plane) and a few
  shader functions, so the shape can be swapped.
- Per-channel renormalization (Jimenez 2015, Golubev).
- Footprint shortcut and per-pixel budgets (HDRP, Xie et al.), in a simple rule.
- Transmission from the same profile, thin (material thickness) and thick (shadow map),
  after Jimenez 2010.
- glTF thickness from `KHR_materials_volume`.

## What we leave (for now)

- Separable blurs: artifacts for non-Gaussian profiles, two passes.
- Texture-space diffusion.
- The pre-integrated LUT tier: measured and deferred (ADR 0059, "Not done").
- Burley's profile: legal question for the team.
