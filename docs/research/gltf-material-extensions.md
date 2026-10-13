# glTF's unlit, clear coat and sheen onto the shading models

ADR 0092's leftover (status §4): glTF's `KHR_materials_unlit`, `KHR_materials_clearcoat` and
`KHR_materials_sheen` imported onto the shading models that step 3 added (unlit, clear
coat, cloth), as built-in material types. Checked 2026-10-13.

## Sources

| Source | Version | License | What we read |
|---|---|---|---|
| Khronos, `KHR_materials_unlit`, `KHR_materials_clearcoat`, `KHR_materials_sheen` (`extensions/2.0/Khronos/*/README.md`) | glTF repository, main, 2026-10-13 | specification | the properties, the BRDFs, the layering |
| Filament gltfio: `libs/gltfio/src/AssetLoader.cpp` (`getMaterialKey`, `createMaterialInstance`), `JitShaderProvider.cpp` | main, 2026-10-13 | Apache 2.0 | which shading model each extension picks, texture channels |
| three.js `examples/jsm/loaders/GLTFLoader.js` (`GLTFMaterialsUnlitExtension`, `…ClearcoatExtension`, `…SheenExtension`) | dev, 2026-10-13 | MIT | the same, for a second engine |
| Bevy `bevy_gltf` (`loader/extensions/khr_materials_clearcoat.rs`; docs.rs table) | 0.20.0 | MIT / Apache 2.0 | what it supports |
| Godot `modules/gltf/gltf_document.cpp` | master 75e2933 | MIT | what it supports |
| Khronos glTF-Sample-Assets: ClearCoatTest, UnlitTest (CC-BY 4.0, Ed Mackey / AGI); SheenChair (CC0, Wayfair), SheenCloth (CC0, Microsoft), SheenTestGrid and ClearcoatWicker (CC0, Eric Chadwick) | main, 2026-10-13 | as listed | test scenes (fetched, never committed) |

No code was copied: the mappings below are from the specifications; the other engines were
read for their choices.

## What the extensions say

- **Unlit:**
  - The color is the base color: factor × texture × vertex color.
  - Alpha coverage and double-sidedness still apply.
  - Everything about lighting is ignored, emission included. Its fallback for clients without the extension sets emission to black.
- **Clear coat:**
  - A specular layer over the whole base material, emission included. The coat uses GGX with α = roughness², IOR 1.5 (F0 0.04) and Fresnel from N·V.
  - Formula: `mix(material, coat_brdf, clearcoat · F_coat)`, and the emission under it is `emission · (1 − clearcoat · F_coat)`.
  - Intensity is `clearcoatFactor` × texture R; roughness is `clearcoatRoughnessFactor` × texture G.
  - The coat's normal comes from `clearcoatNormalTexture` (with its scale). Without that texture the coat has no normal map, even when the base has one.
- **Sheen:**
  - A layer on top of the base metallic-roughness material: GGX stays.
  - Formula: `sheen_color · sheen_brdf + material · (1 − max3(sheen_color) · E(N·V))`, the simplified form with N·V only. E is the sheen lobe's directional albedo.
  - The BRDF is the Charlie distribution (α = roughness²) with the Charlie visibility (a fitted λ); Ashikhmin's (Neubelt's) visibility is allowed.
  - Color is `sheenColorFactor` × texture RGB (sRGB); roughness is `sheenRoughnessFactor` × texture A.
  - A coat, when present, lies over the sheen.

## How each engine does it

- **Filament gltfio:**
  - Unlit → `Shading::UNLIT`: base color × vertex color, no emission.
  - Clear coat and sheen are properties of `Shading::LIT`, together or apart:
    - coat strength from texture R, coat roughness from G;
    - `clearCoatNormal` from its own map × its scale;
    - sheen color from RGB, sheen roughness from A.
  - Its `CLOTH` model is never used for glTF. Lit's sheen scales the base by `1 − max3(sheenColor) · DFG_cloth`, where DFG_cloth is Charlie with Neubelt, from the DFG table's third channel.
- **three.js:** unlit → `MeshBasicMaterial` (no emission). Clear coat and sheen → `MeshPhysicalMaterial`, a lit material with both layers.
- **Bevy 0.20:** unlit and clear coat supported (the coat's textures behind the `pbr_multi_layer_material_textures` feature); sheen not supported.
- **Godot (master):** unlit → `SHADING_MODE_UNSHADED`; neither clear coat nor sheen is imported.

## What we take

- **Built-in material types** (ADR 0092: "the shading models, as built-in types where glTF has them"), in an engine module with no entry points (`shaders/materials/gltf.slang`).
  - The import marks the materials that use the extensions. A step like the material files' turns them into typed materials, with their glTF record as the fallback, laid out through Slang's reflection, compiled once per type, and cooked into packs with their SPIR-V.
  - The engine's own glTF pipelines don't change.
- **Unlit** → `MODEL_UNLIT`:
  - base color × texture, alpha and double-sidedness as glTF's;
  - no emission (as the specification, Filament and three.js);
  - no vertex colors (the importer reads none yet).
- **Clear coat** → `MODEL_CLEARCOAT`, Filament's coat, which is what Filament's importer uses too.
  - Strength and roughness are factor × texture, from R and G.
  - The coat's normal comes from `clearcoatNormalTexture` (a new `Surface.clearcoat_normal`), otherwise the interpolated normal as before.
  - Emission is darkened under the coat as the specification says (`1 − clearcoat · F`).
- **Sheen** → a new model, **lit with a sheen layer** (`MODEL_SHEEN`): glTF's metallic-roughness scaled by `1 − max3(sheen) · E(N·V)`, plus Charlie with Neubelt.
  - E is the lobe's albedo fit cloth already has (`sheen_albedo`), so the scaling matches our lobe, as Filament's DFG_cloth matches its.
  - Not `MODEL_CLOTH`: cloth drops GGX, and Filament keeps the two apart as well.
- **Sheen with clear coat** → `MODEL_SHEEN_CLEARCOAT`: the coat over the sheen layer.

## What we do not take

- **UV sets other than the first, and `KHR_texture_transform`** on these textures: the importer reads `TEXCOORD_0` only. A texture on another set is dropped with a warning (the factor stays).
- **The specification's Charlie visibility with its fitted λ:** Neubelt's is allowed. It is what cloth already uses, and what our albedo fit was made for.
- **Unlit with vertex colors:** that waits for vertex colors in general.
