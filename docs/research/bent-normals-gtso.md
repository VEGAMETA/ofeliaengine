# Bent normals and cone-based specular occlusion (GTSO) from GTAO

Queue item 3d (owner, 2026-10-10: after GI): directional ambient occlusion and specular
occlusion from the visible cone, so that ambient light comes from the unoccluded direction
and environment reflections fade where the visible cone does not cover the reflection.
Indoors without GI, diffuse ambient comes from the sky's harmonics, barely occluded, while
dielectric reflections keep the sky's brightness under arches: dielectrics look too bright
in shade next to metals (status §6).

Checked 2026-10-13.

## Sources

| Source | Version | License | What we read |
|---|---|---|---|
| J. Jimenez, X. Wu, A. Pesce, A. Jarabo, "Practical Realtime Strategies for Accurate Indirect Occlusion", SIGGRAPH 2016 course | 2016 | paper | Algorithm 2 (bent normals from the horizons), GTSO (specular occlusion from the visibility cone and the specular cone) |
| Intel XeGTAO, `Source/Rendering/Shaders/XeGTAO.hlsli` | master a5b1686 | MIT | `XE_GTAO_COMPUTE_BENT_NORMALS`: the per-slice bent normal integral, its rotation from the slice frame, R8G8B8A8 packing of bent normal and visibility, the denoiser averaging both and renormalizing |
| Filament, `shaders/src/surface_ambient_occlusion.fs` | main 764fe6c | Apache 2.0 | `SpecularAO_Cones`: visibility aperture `cos = √(1 − V)`, specular aperture `exp2(−3.32·α²)`, Oat and Sander's spherical-cap intersection, specular occlusion faded out below roughness 0.1 |
| C. Oat, P. Sander, "Ambient Aperture Lighting", I3D 2007 | 2007 | paper | the smooth approximation of two spherical caps' intersection |
| Unreal Engine, "Bent Normal" maps and GTAO (product documentation) | 5.x | read as a product | bent normals for diffuse ambient and specular occlusion from baked or screen-space AO |

No code was copied: XeGTAO's integral and packing are formulas (credited in
THIRD_PARTY_NOTICES.md with XeGTAO already); Filament's cone intersection is re-derived from
the two papers.

## How each does it

- **XeGTAO / the paper:** per slice, with the horizon angles `h0`, `h1` and the projected
  normal's angle `n`, the cosine-weighted visible arc's mean direction in the slice plane
  has closed forms (`t0` along the slice's screen direction, `t1` along the view vector);
  summed over slices, weighted by the projected normal's length, normalized. Cost: XeGTAO
  quotes about +25 % of the main pass. Output: bent normal and visibility in one RGBA8
  (8 bits each), view space (we turn it to world space before writing: see below).
- **Filament:** bent normals (from its SAO, or from bent normal maps) shade diffuse ambient
  along the bent normal, and specular occlusion is the fraction of the specular cone inside
  the visibility cone around the bent normal (the paper's GTSO, with Oat and Sander's
  approximation instead of the paper's 4D table).
- **Unreal:** the same use of bent normals; GTSO from its GTAO.

## What we take

(As built, ADR 0058's addendum; the measurements are there.)

- XeGTAO's bent normal integral in our slice frame (x right, y up, z forward): `t0` along
  the slice direction orthogonal to the view vector, `t1` along the view vector, weighted
  by the projected normal's length; normalized after the slices.
- RGBA8: the visibility in R (shading without bent normals reads R as before) and the bent
  normal (turned to world space in the AO pass, × 0.5 + 0.5) in GBA; the denoiser blurs all
  four with the same weights and renormalizes the normal. XeGTAO's `t0`/`t1` rewritten
  without trigonometry from the horizons' cosines.
- In the shading that reads it only, behind its own switch and entry points (the register
  rule, ADR 0058): diffuse ambient from the harmonics along the shading normal bent as the
  geometry's was (`normalize(n + bent − n_geometric)`); specular occlusion from the cones
  instead of Lagarde's fit, with Oat and Sander's smooth overlap taken in the cosines of
  the angles (no arc cosines).

## What we do not take

- The paper's 4D lookup table for the cone intersection: Oat and Sander's approximation is
  within a few percent of it (Filament, Jimenez) and costs no texture read.
- Bent normals in GI's diffuse light: probe GI resolves its own irradiance per pixel.
