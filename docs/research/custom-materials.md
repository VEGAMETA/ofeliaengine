# Custom 3D materials and shaders

Queue item 3c (`docs/status.md`; review 2 §4: "the biggest missing core piece for real
games"). CLAUDE.md §9 asks for a material graph that generates Slang and hand-written Slang
with full support, permutations kept small, "Compile All Shaders", PSO collection and no
hitches for players. Checked 2026-10-12.

## Where the renderer stands

- **One material model:** glTF's metallic-roughness with subsurface scattering (ADR 0040,
  ADR 0059). Every material is 80 bytes of factors and bindless texture indices in one GPU
  table, read per instance (`engine_render::material`). No code per material.
- **Shading variants are entry points with literal flags.** `forward.slang`'s
  `shade_surface` takes `masked`, `ao`, `shadows`, `contact`, `decals`, `gi`, `subsurface`,
  `reflective` as literals. Each combination a view uses is its own entry point (105 in
  `forward.slang`), so a feature a view does not use costs it no registers (the register
  rule, CLAUDE.md §5: on RDNA2 dead code under a specialization constant took forward
  shading from 64 to 72 VGPRs).
- **Pipelines are described up front and made on first use or by a warm-up thread**
  (ADR 0079), from a persistent pipeline cache with our own file (ADR 0055).
- **Draws are GPU-driven** (ADR 0064): batches culled on the GPU, one indirect draw per
  pipeline and phase. A new pipeline is a new batch.

## Measured on ref-1 (2026-10-12)

| | value |
|---|---|
| Compile `forward.slang`, 105 entry points, `-O`, one thread (Slang 2026.18.3) | 7.7 s (≈ 74 ms per entry point) |
| Compile one entry point of it (the rest of the module parsed, not emitted) | 0.26 s |
| One shading pipeline made with the drivers cold (ADR 0079) | 26–35 ms |
| The forward renderer's 33 variants, cold, one after another (ADR 0079) | 1,145 ms RTX / 867 ms Radeon |
| The same from the pipeline cache file | 37 ms / 2 ms |

**The prototype** (a copy of `forward.slang`, not in the repository): the material's
part of `shade_surface` behind a Slang interface `IMaterial { static Surface evaluate(..) }`,
glTF's code moved into `struct GltfMaterial : IMaterial`, and the lighting in a generic
`shade_generic<M : IMaterial>`. Optimized SPIR-V:

| entry point | bytes | instructions | image samples |
|---|---:|---:|---:|
| `fragmentMain` (today's) | 33,196 | 1,754 | 8 |
| `fragmentMainGeneric` (`GltfMaterial` through the interface) | 33,200 | 1,753 | 8 |
| `fragmentStripes` (a procedural material through the same interface) | 29,492 | 1,528 | 3 |

The first two differ only in the order of three constants and the entry point's name:
Slang specializes the generic at compile time, so the interface costs nothing, and a custom
material pays only for what its own code does.

**What it means for scale.** A material compiled into every forward entry point would cost
about 7.7 s of compiling and, with the drivers cold, 33 × 30 ms ≈ 1 s of pipelines; 100
materials, 13 minutes on one thread and 100 s of pipelines. So only the variants a material
is actually drawn with may be compiled and made: in a view, a material needs one shading
variant (two with masked alpha) and shares the engine's depth and shadow pipelines unless
it moves its vertices or discards.

## How other engines do it

| Engine (license, version checked) | Authoring | Variants | Hitches |
|---|---|---|---|
| **Unreal Engine 5** (source-available, not open; docs 2026-10-12) | material graph → HLSL; custom HLSL nodes | per material × vertex factory (static, skeletal, Niagara…) × pass × static switches and quality levels; `ShouldCompilePermutation` on shader, material and vertex factory filters them | PSO precaching and bundled PSO caches; known for permutation counts in the tens of thousands |
| **Unity** (closed; docs) | Shader Graph → HLSL; hand-written ShaderLab | keywords (`multi_compile` / `shader_feature`), variant stripping at build | Shader variant collections, warm-up |
| **Godot 4.4** (MIT; release notes 2025-03) | its own shader language per material (and visual shaders) | one shader per material, variants by render features through specialization constants | **ubershaders**: each shader also compiled with its specialization constants as uniforms at load; that pipeline draws while the specialized one compiles in the background (Steam Deck first-frame stutter 320 → 30 ms) |
| **Filament** (Apache-2.0, `Variant.h` on main, 2026-10-12) | material files compiled offline by `matc` (a `material()` function filling `MaterialInputs`) | 7 variant bits (directional light, stereo, shadow receiver, skinning, depth, fog / picking, sampler / VSM): 128 slots, 61 used; vertex and fragment bits filtered apart; materials can filter variants | all variants compiled offline into the material package |
| **Bevy 0.20** (MIT/Apache, docs.rs 2026-10-12) | `Material` trait: WGSL vertex/fragment, prepass and deferred shaders; `ExtendedMaterial` changes the standard PBR input before lighting | pipelines specialized by a mesh key (bits) × the material's key (`specialize`) | pipelines made on first use; render methods forward, deferred, and meshlets (material shaded over a visibility buffer) |
| **Slang** (MIT; He et al., "Slang: language mechanisms for extensible real-time shading systems", SIGGRAPH 2018) | interfaces and generics for materials and lights; link-time specialization | specialization by type arguments instead of the preprocessor | — |

What they agree on:

- **The material writes a surface, the engine lights it** (Filament's `MaterialInputs`,
  Bevy's PBR input, Unreal's material attributes, Slang's `IMaterial` pattern). Shading
  models (lit, unlit, subsurface, cloth…) are the engine's, picked per material at compile
  time.
- **Variants multiply** (material × pass × features × mesh type): every engine filters
  them, and those that compile on first use stutter unless something hides it (PSO
  collections, warm-up, Godot's ubershader fallback).
- **One material, several render paths** (Bevy: forward, deferred, meshlets; Unreal:
  Nanite's material pass): the material's code does not know which one runs it.

## What we take

- **The material is a Slang type implementing `IMaterial`**: it turns the interpolants
  and its parameters into a `Surface` (base color, normal, roughness, metallic,
  occlusion, emission, alpha, the shading model's extras). Optionally it moves vertices
  (wind on foliage) through a second function. The engine lights the surface.
- **The built-in glTF material becomes the first `IMaterial`**: same code, same SPIR-V
  (measured above), so nothing gets slower.
- **Specialization by generics, never by branches** (the register rule): entry points
  become generic over the material; Slang emits one per (material, variant) actually used.
- **Texture sampling through a context** the material receives (`ctx.sample(texture, uv)`):
  implicit derivatives in a fragment shader, analytic ones in a compute material pass. So
  the same material can later run over a visibility buffer (ADR 0085) without changes.
- **Only used variants are compiled and made:** the scene's materials × the views'
  feature sets actually drawn, collected while playing (PSO collection, CLAUDE.md §9),
  compiled by "Compile All Shaders", warmed up from the pipeline cache (ADR 0055, 0079).
- **Godot's lesson for the first sight of a material:** something must draw while its
  pipeline compiles. Our candidate is the built-in glTF shading with the material's
  declared fallback parameters, not an ubershader (an ubershader would break the register
  rule only for the frames it draws, but it is a second shading path to keep exact).

## What we do not take

- **Unreal's and Unity's permutation scale** (static switches × quality levels × vertex
  factories): our mesh types are one vertex format, skinning runs in compute before the
  passes (ADR 0068), so the vertex factory axis does not exist.
- **Deferred shading now**: a G-buffer costs bandwidth the 2-CU iGPU does not have
  (its memory-bound passes, `reference-hw.md`), loses MSAA (ADR 0075) and transparency
  goes forward anyway. The visibility buffer stays ADR 0085's later step.
- **Materials as data only** (one uber material with more parameters): CLAUDE.md §9 asks
  for custom code, and every new parameter would be paid for by every material.

No code was copied from any of these engines; Unreal and Unity were read as products and
documentation only.

## Shading models (ADR 0092 step 3, 2026-10-12)

The owner asked for lit, unlit, clear coat, cloth, hair and toon in the first version.
How others light them, and what we take:

| Model | Filament | Godot 4 | Unreal / Unity (products) | Ours |
|---|---|---|---|---|
| Clear coat | a second GGX lobe, Kelemen visibility, Schlick at 0.04 (IOR 1.5), on the geometric normal under a normal map; the base (Fd + Fr) × (1 − Fc); in the code the base is made at least as rough as the coat ("a hack but it will do") and its f0 recomputed against the coat (a polynomial fit of the exact formula); the coat's IBL lobe × Fc's attenuation of the base | `clearcoat`: a GGX lobe on the vertex normal (normal maps ignored), Kelemen visibility, Schlick at 0.04, roughness remapped to 0.001–0.1 | Unreal "Clear Coat"; Unity HDRP "coat mask" | Filament's lobe, base roughness and base f0, the f0 by the exact formula (the IOR from f0, then Fresnel against 1.5) |
| Cloth | Charlie distribution (Estevez and Kulla 2017; their shadowing term omitted for its cost), Neubelt visibility (Neubelt and Pettineo 2013), no Fresnel: the sheen color (default 0.04) instead; optional subsurface color (a wrapped diffuse); IBL through "a single DG term in the 3rd channel of the DFG LUT" | none (rim and "backlight" only) | Unreal "Cloth" (fuzz); Unity HDRP "Fabric" (Charlie) | Filament's lobe without the subsurface color; the sky's sheen through an analytic fit of the lobe's albedo (below), not a third LUT channel |
| Hair | none | none | Unreal: Marschner-based; Unity HDRP: Kajiya-Kay or Marschner | Kajiya-Kay with Scheuermann's two shifted highlights, Fresnel-weighted, Blinn-Phong-normalized |
| Toon | none | `diffuse_toon`: smoothstep(−roughness, roughness, n·l) / π; `specular_toon`: a smoothstep of r·v around (1 − roughness)², added to the diffuse light | Unity's toon shaders (bands, high color, rim) | bands of n·l starting at 0 (where the shadow test cuts), a flat highlight from GGX's half-peak, a rim |
| Unlit | `unlit` model | `unshaded` | Unreal "Unlit" | the color and emission, an early return |

- **Marschner's hair** (2003, as in Unreal) is the physically based alternative: three
  lobes with azimuthal scattering, more instructions and a LUT or fits. Kajiya-Kay first;
  Marschner when a game needs it.
- **Wrapped diffuse for hair** was tried and dropped: the sun's shadow test cuts at
  n·l = 0, so a wrap ends in a step there. Toon's bands start at n·l = 0 for the same
  reason.
- **The clear coat's reflection is the one the reflections replace** (ADR 0073): the
  composite swaps one lobe per pixel, and the coat's sharp one is what shows on car paint.
  The base's lobe keeps the sky.

### Cloth's sheen in the sky light: a fit instead of a LUT channel

Filament integrates the cloth lobe into a third channel of its DFG table. Ours is
`rg16f`, read by every lit pixel; widening it would cost every material for one model.
Instead the lobe's directional albedo (F = 1),

E(μ, r) = ∫ D_Charlie(n·h; α) · V_Neubelt(μ, n·l) · n·l dω_l, α = max(r, 0.07)²,

was integrated numerically (384 × 192 directions, μ in [0.005, 1] × r in [0.07, 1], 33 ×
32 points) and fitted (Levenberg-Marquardt, 9 parameters) as

E ≈ min(a(r) · exp(−b(r) · μ^c(r)), 1),
a = 1.42617 − 1.68309 r + 0.78282 r², b = 0.08726 + 1.09762 / r + 0.10519 / r²,
c = 1.78444 − 2.16779 r + 1.60655 r²,

with the integral clamped at 1 (Neubelt's visibility is not energy-conserving at grazing
angles: E reaches 1.54 there). Error over the grid: at most 0.043, root mean square 0.008.
Two other forms (a rational one, an exponential of a quadratic in μ) fitted worse (0.064,
0.079). A sample of the integral:

| r \ μ | 0.05 | 0.2 | 0.5 | 0.9 | 1.0 |
|---|---|---|---|---|---|
| 0.1 | 1.185 | 0.234 | 0.001 | 0.000 | 0.000 |
| 0.3 | 0.897 | 0.516 | 0.137 | 0.008 | 0.001 |
| 0.6 | 0.629 | 0.470 | 0.252 | 0.086 | 0.056 |
| 1.0 | 0.514 | 0.422 | 0.287 | 0.168 | 0.142 |

The first version scaled the sky's GGX DFG by the sheen color: about 0.6 at every angle for
the test's sheen, washing the whole sphere white instead of brightening its silhouette.

### Sources (shading models)

- Filament, "Physically Based Rendering in Filament" (google.github.io/filament/Filament.md.html,
  read 2026-10-12): clear coat, cloth and cloth IBL sections; its shaders at main 764fe6c
  (`shaders/src/surface_shading_lit.fs`, `surface_shading_model_standard.fs`,
  `surface_light_indirect.fs`, `surface_material.fs`), read 2026-10-12. Apache 2.0;
  formulas re-derived, no code copied.
- A. Estevez, C. Kulla, "Production Friendly Microfacet Sheen BRDF", Sony Pictures
  Imageworks, 2017.
- D. Neubelt, M. Pettineo, "Crafting a Next-Gen Material Pipeline for The Order: 1886",
  SIGGRAPH 2013 course.
- C. Kelemen, L. Szirmay-Kalos, "A Microfacet Based Coupled Specular-Matte BRDF Model with
  Importance Sampling", Eurographics 2001.
- J. Kajiya, T. Kay, "Rendering Fur with Three Dimensional Textures", SIGGRAPH 1989.
- T. Scheuermann, "Hair Rendering and Shading", GDC 2004 / ShaderX3.
- S. Marschner et al., "Light Scattering from Human Hair Fibers", SIGGRAPH 2003 (not
  taken yet).
- A. Lake et al., "Stylized Rendering Techniques for Scalable Real-Time 3D Animation",
  NPAR 2000.
- Godot, `servers/rendering/renderer_rd/shaders/scene_forward_lights_inc.glsl` at master
  bd02272 (read 2026-10-12): clear coat and the toon modes. MIT; read for ideas, no code
  copied.

## Sources

- Filament, `libs/filabridge/include/private/filament/Variant.h` (main, read 2026-10-12),
  github.com/google/filament.
- Godot 4.4 release notes, godotengine.org/releases/4.4; PR #90400 "Ubershaders and
  pipeline pre-compilation" (DarioSamo).
- Bevy 0.20 `Material` trait, docs.rs/bevy/latest/bevy/pbr/trait.Material.html (read
  2026-10-12).
- Unreal Engine, "Shader Development in Unreal Engine",
  dev.epicgames.com/documentation/en-us/unreal-engine/shader-development-in-unreal-engine;
  M. Ned, "Unreal Engine 4 Rendering Part 2: Shaders and Vertex Data".
- Y. He, K. Fatahalian, T. Foley, "Slang: language mechanisms for extensible real-time
  shading systems", ACM TOG 37(4), SIGGRAPH 2018.
- Our measurements: ADR 0055, ADR 0079, the prototype above.
