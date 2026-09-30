# Image-based lighting: light from the whole sky

Checked 2026-09-30. Sources at the end. No code was copied; the formulas come from the
papers and documentation.

## Why

The forward renderer's ambient term is a flat color times albedo:
- metals are black away from the sun;
- rough dielectrics look flat;
- the background is the clear color.

Light from the whole environment, diffuse and specular, is what makes PBR materials
read correctly, and what the sky contributes at every point.

## How others do it

- **Split sum** (Karis, "Real Shading in Unreal Engine 4", SIGGRAPH 2013). The specular
  integral of radiance × BRDF is split into two factors:
  - radiance prefiltered with the GGX lobe, one roughness per mip of a cube map, assuming
    n = v = r;
  - a 2D table over (n·v, roughness) of the BRDF's integral (scale and bias of F0).
  - Prefiltering importance-samples GGX.
- **Filament** (Apache-2.0, "Physically Based Rendering in Filament" and its source):
  - the DFG table stores ∫ Gv·Fc and ∫ Gv, so specular = prefiltered × mix(x, y, f0);
  - multiple-scattering energy compensation, 1 + f0 · (1/y − 1), restores what
    single-scattering GGX loses on rough metals;
  - diffuse irradiance from 3-band spherical harmonics;
  - specular occlusion from the ambient occlusion.
- **Filtered importance sampling** (Křivánek and Colbert, GPU Gems 3, 2007): each
  sample reads the source mip whose texel solid angle matches the sample's, so few
  samples give smooth prefiltered maps.
- **Irradiance by spherical harmonics** (Ramamoorthi and Hanrahan, "An Efficient
  Representation for Irradiance Environment Maps", SIGGRAPH 2001): the clamped cosine
  keeps 99% of its energy in bands 0–2, so 9 coefficients per color give irradiance with a
  few percent of error for any environment. The convolution multiplies bands by π, 2π/3
  and π/4.
- **Specular occlusion** (Lagarde and de Rousiers, "Moving Frostbite to PBR", 2014):
  saturate((n·v + ao)^exp2(−16α − 1) − 1 + ao).
- **Bevy** (MIT/Apache): environment map lights from prefiltered KTX2 cube maps (generated
  offline or at run time), the same split sum.
- **Godot** (MIT): radiance cube maps generated from the sky shader at run time, updated
  incrementally.

## Conclusions

1. **An `Environment`** keeps, across frames:
   - a radiance cube (128², full mip chain);
   - a prefiltered cube (128², 6 roughness mips);
   - 9 SH irradiance coefficients;
   - a 64² DFG table.
   It rebuilds them on the GPU only when the sky changes.
2. **The sky** is a three-color gradient (zenith, horizon, ground) for now. A physically
   based atmosphere (Hillaire 2020) is the next step and fills the same cube.
3. **The forward shader,** with an environment:
   - irradiance from the SH;
   - prefiltered radiance at the reflected direction and roughness;
   - the DFG table with Filament's compensation;
   - specular occlusion.
   A background pass draws the sky and the sun's disk.
4. **Tests against the CPU:**
   - a white furnace (metals must reflect exactly 1);
   - the DFG integral;
   - the irradiance of a gradient;
   - the cube's orientation through a debug sky.

## Sources

- Karis, "Real Shading in Unreal Engine 4", SIGGRAPH 2013 course notes
- Filament, "Physically Based Rendering in Filament", image-based lights (https://google.github.io/filament/Filament.md.html), checked 2026-09-30 (Apache-2.0)
- Křivánek, Colbert, "Real-time Shading with Filtered Importance Sampling", EGSR 2008; GPU Gems 3, chapter 20
- Ramamoorthi, Hanrahan, "An Efficient Representation for Irradiance Environment Maps", SIGGRAPH 2001
- Lagarde, de Rousiers, "Moving Frostbite to Physically Based Rendering 3.0", SIGGRAPH 2014 course notes
- Duff et al., "Building an Orthonormal Basis, Revisited", JCGT 2017
- Hillaire, "A Scalable and Production Ready Sky and Atmosphere Rendering Technique", EGSR 2020 (next step)
