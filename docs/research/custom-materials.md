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
