# 0092. Custom materials: Slang types that write a surface, specialized into the passes that draw them

- Status: Accepted (2026-10-12, the owner: "agree with everything"; ubershaders to be tried;
  more shading models in the first version)
- Date: 2026-10-12
- Builds on:
  - [ADR 0040](0040-materials-and-textures.md): the material table;
  - [ADR 0055](0055-pipeline-cache-and-warm-up.md), [ADR 0079](0079-pipelines-on-first-use.md):
    pipelines;
  - [ADR 0085](0085-visibility-buffer-and-meshlets.md): the visibility buffer is decided
    together with this.
- Research: [docs/research/custom-materials.md](../research/custom-materials.md)

## Context

The renderer has one material model: glTF's metallic-roughness with subsurface scattering.
A game needs its own:

- procedural surfaces;
- water, glass, cloth, toon shading;
- wind on foliage, dissolves, decals' own looks.

CLAUDE.md §9 asks for:

- hand-written Slang with full support and a material graph generating Slang;
- few permutations;
- "Compile All Shaders";
- PSO collection;
- no hitches for players.

How custom materials enter the shaders decides how every pass is built and how many
pipelines exist. That is core architecture, not tooling (review 2 §4).

Two facts constrain it (research note):

- **Variants multiply.** A material compiled into all 105 forward entry points costs 7.7 s
  of Slang and about 1 s of cold pipelines. For 100 materials that is 13 minutes and
  100 s.
- **The register rule** (CLAUDE.md §5): no feature may cost registers where it is unused.
  Branches on material type would break it.

## Options

### A. Where the material's code runs

1. **Specialized into the forward passes** (Filament, Bevy's forward path, Unreal,
   Godot).
   - The material is a Slang type; the passes' entry points become generic over it.
   - Slang emits one per (material, variant) used.
   - Measured: the built-in material through the interface gives the same SPIR-V as
     today (33,200 against 33,196 bytes, 1,753 against 1,754 instructions, the same 8
     samples).
   - A custom material pays only for its own code.
2. **Deferred: materials write a G-buffer, lighting is shared.**
   - Variants add up (materials + features) instead of multiplying.
   - But it costs G-buffer bandwidth, which the 2-CU iGPU lacks, and it loses MSAA
     (ADR 0075).
   - Transparent materials stay forward anyway.
3. **A visibility buffer now** (ADR 0085's later step).
   - Materials are shaded in compute per tile class.
   - The same additive variant count as option 2 without the G-buffer's bandwidth.
   - A large change before any material exists; MSAA needs its own path.
4. **Data-only materials:** one uber material with more parameters.
   - No custom code, against CLAUDE.md §9.
   - Every parameter is paid for by every material.

### B. What draws a material whose pipeline is not made yet

1. **Wait** (a hitch on first sight; today's behavior for the built-in variants, hidden
   by the warm-up thread).
2. **Godot 4.4's ubershader:** a pipeline with the features as uniforms draws until the
   specialized one is ready. It is a second shading path to keep exact.
3. **The built-in glTF shading with the material's declared fallback parameters** (a
   color, roughness, metallic, its textures if it names them) draws until the material's
   own pipeline is ready. No hitch; the material looks plain for a few frames.

## Decision

### 1. The material interface

- **A material is a Slang type implementing `IMaterial`.** It computes a `Surface` from
  the interpolants and its parameters:
  - base color and alpha, normal, roughness, metallic, occlusion, emission;
  - the shading model's extras.
- **Optionally it moves vertices** (`IMaterialVertex`: an offset in world space, with its
  motion for motion vectors).
- **It samples textures through a context** it receives (`ctx.sample(texture, uv)`):
  - implicit derivatives in fragment shaders;
  - analytic ones in a compute pass.

  So the same material later runs over a visibility buffer unchanged.
- **The engine lights the surface.** Shading models are the engine's, chosen per material
  at compile time (a static constant of the type), each its own code path, never a branch.
  The first version has (the owner, 2026-10-12): lit (today's, subsurface included),
  unlit, clear coat, cloth (sheen), hair and toon.
- **The built-in glTF material becomes `GltfMaterial : IMaterial`.** It is the same code
  with the same SPIR-V, the default, and the importer's output.

### 2. Parameters

- **A material's parameters are a struct of its type.** The engine reads the layout
  through Slang's reflection: factors, colors, texture references (bindless indices with
  their sampler bits, ADR 0040).
- **They live in the material table,** as today's 80 bytes do. A slot holds its type's
  struct, so the table's stride becomes the largest type's (padded to 16 bytes, with a
  cap, 256 bytes to start).
- **No descriptor sets per material,** no rebinding between draws.

### 3. Variants, pipelines and draws

- **Entry points are generic over the material** (option A.1): Slang specializes them.
- **Only the (material, view features) pairs drawn are compiled and made:**
  - "Compile All Shaders" (`ofeliaengine shaders build` and the editor) compiles the
    pairs a project's PSO collection recorded;
  - the editor and playtests record them;
  - with no record yet: the materials × the default feature sets of each device class.
- **Materials share the engine's depth and shadow pipelines** unless they move vertices
  or discard (alpha mask, dissolves): those get their own depth, shadow and motion
  variants.
- **Batches:** one indirect draw per pipeline as today; the GPU culling sorts instances
  into per-material-type batches.
- **First sight** (B.3): the glTF shading draws with the material's fallback parameters
  while the pipeline compiles on the warm-up thread. Never a hitch; a plain look for a few
  frames when no PSO record or cache covers it.
- **Ubershaders are tried after** (the owner, 2026-10-12): a material's pipeline with the
  view features as uniforms, made at load, drawing until the specialized one is ready
  (B.2). It replaces B.3 if it measures right on both GPUs: its cost while it draws, its
  compile time, and the same picture as the specialized pipeline.
- **Hot reload** (budget 500 ms, CLAUDE.md §5): editing a material recompiles its pairs in
  use, 0.26 s per entry point measured, in parallel; then the pipelines are swapped. A
  compile error keeps the previous version and is shown at its line.

### 4. Authoring and assets

- **Hand-written Slang first:** a `.slang` module with the type, its parameter struct and
  its fallback.
- **The material graph** (editor tooling, after the core) generates the same kind of
  module.
- **Material instances** (one shader, different parameters and textures) are assets: a
  text file naming the shader module and setting its parameters, cooked into packs. Its
  exact format is the owner's question 3.

### 5. Later, not in this step

- The visibility buffer's material pass (ADR 0085), with the same `IMaterial` types.
- The node graph editor.
- Shading models beyond the first version's six.

## The owner's answers (2026-10-12)

1. Forward, specialized per material, the visibility buffer later with the same materials:
   yes.
2. First sight drawn plain with the built-in shading: yes; and try ubershaders.
3. The material instance file: a RON text file, cooked into packs: yes.
4. Shading models: lit, unlit, clear coat, cloth, hair and toon.

## Step 1 as built (2026-10-12)

- `forward.slang`: `MaterialContext`, `Surface`, `IMaterial` and `GltfMaterial`;
  `shade_surface<M : IMaterial>`, and the masked passes through `M::alpha`.
- **The interface's shape is measured, not free.** A first version computed the whole
  surface (occlusion and emission included) in one function, reading the material's
  fields where each was needed. The SPIR-V was the same size, but the Radeon's compiler
  gave several variants more registers (`scene_bench --sky --pipeline-stats`: environment
  64 → 70–72 VGPRs, lights + environment 80 → 90, transparent under the atmosphere 78 → 88),
  and Sponza's textured forward pass took 8 % longer.
- **As built:** the material's parameters are an associated type `Params`, loaded once at
  the top (`M::load`, as the material record was before). Occlusion, emission and
  thickness are functions of their own, called where the lighting used them. Every
  pipeline's shader properties are now identical on the Radeon (VGPRs, SGPRs, LDS,
  scratch). 103 of the 105 forward entry points have SPIR-V of the same size; the
  alpha-to-coverage prepasses are one instruction shorter.

## Steps

1. The passes through `IMaterial`, with `GltfMaterial` the only type: the same SPIR-V,
   golden images and registers. (Done, PR #179.)
2. Material types in the renderer (taken before the shading models, which need types to be
   drawn and tested). (Done, PR #180.)
3. The shading models, as built-in types where glTF has them (unlit, clear coat, sheen for
   cloth) and as engine types (hair, toon). (Done, PR #181, as a property of any type; glTF's
   extensions onto them come with the material file, below.)
4. The material file, cooking, "Compile All Shaders" over PSO records, hot reload.
5. The ubershader experiment against the plain fallback.

### Step 2's design

- **One source for every type.** `forward.slang`'s entry points shade with `MATERIAL`, a
  macro that defaults to `GltfMaterial`; a type's entry points are the same file compiled
  with `-D MATERIAL=<its type>` and its module included (`MATERIAL_FILE`). The built-in
  SPIR-V does not change; a type compiles only the entry points it is drawn with.
- **A type's pipeline is the built-in pipeline's description with the type's fragment
  module** (`LazyPipeline::with_fragment`): blending, depth, formats and specialization
  stay the engine's, so the pipeline sets of every feature (ambient occlusion, shadows,
  reflections, decals, GI, MSAA) need no copy per type. The vertex stage stays the
  engine's (vertex offsets come later, with their own depth, shadow and motion variants).
- **Every material keeps a glTF record** in the material table: its fallback. It draws the
  material while the type's pipelines are not ready (first sight), and the passes that do
  not run material code (GI's ray hits, ADR 0088) read it. A type's own parameters are
  bytes in a separate buffer, found through a per-slot offset (`Frame.material_offsets`,
  `Frame.material_data`) that only the types' code reads: the built-in path is unchanged.
- **Batches are keyed by (mesh, state, type)**: commands are laid out by type then state;
  each type's batches are drawn with its pipelines, or with the built-in ones until its
  pipelines are made.

### Step 2 as built (2026-10-12)

- **Shaders.** `forward.slang`'s entry points shade with `MATERIAL` (default
  `GltfMaterial`); `material_params<T>(ctx)` reads a type's parameters
  (`Frame.material_data`, `Frame.material_stride`, 256 bytes per slot).
- **The material table** (`MaterialTable::add_typed`, `MaterialParams`): a material keeps
  its glTF record (the fallback) and its type's parameter bytes, with texture references
  resolved like glTF's; the parameters' buffer exists once a typed material does.
- **Batches and draws.** `MaterialKind::ty`; the GPU scene's batches are keyed by type;
  the layout sorts a state's runs by type (blended runs stay back to front across types)
  and keeps up to 128 typed command ranges per frame (`TypedRanges`); a pass draws each
  with its type's pipeline (`TypedPipelines`) or the built-in one.
- **Pipelines.** `LazyPipeline::with_fragment` derives a type's pipeline from the engine's
  ("role"); `forward::material_types` resolves them while a frame is declared, queues the
  missing ones, and in `FrameRenderer::prepare` sends the entry point to a compiler
  thread (Slang, `engine_shader`) and describes the pipeline for the warm-up thread once
  its SPIR-V is back. A frame never waits.
- **Measured:**
  - **The built-in path is unchanged:** every pipeline's shader properties are identical
    on the Radeon (`scene_bench --sky --pipeline-stats`).
  - **The golden shot `procedural_material_type`** (the procedural scene's floor as a
    checker type, `tests/golden/materials/stripes.slang`) is the same bytes on every run
    on both GPUs.
  - **The first sight draws the fallback:** the first frame is the plain scene's, but for
    2 pixels on the Radeon where depths tie (`a_material_type_first_draws_its_fallback`).
- **An AMD compiler bug found on the way.** Reading a struct through a pointer cast
  (`OpBitcast`) from `uint*` after `OpPtrAccessChain` returns zeros on AMD's driver
  26.8.1; NVIDIA's reads right. The parameters' address is therefore computed as a 64-bit
  integer (`OpConvertUToPtr`). This needs `shaderInt64`, now enabled where the adapter
  has it (both reference GPUs do). Without it, types draw their fallbacks (logged).
- **Not yet** (later steps):
  - masked types' own alpha in the depth and shadow passes (they use the fallback's);
  - vertex offsets;
  - precompiled SPIR-V for shipped games (types compile at run time from the engine's
    shader folder);
  - the material file;
  - hot reload of a type's module.

### Step 3 as built (2026-10-12)

- **A model is a type's property:** `IMaterial::model()` returns `MODEL_LIT`, `MODEL_UNLIT`,
  `MODEL_CLEARCOAT`, `MODEL_CLOTH`, `MODEL_HAIR` or `MODEL_TOON`, a literal. `Surface`
  gained the models' terms (coat, sheen, strand tangent and shift, toon bands, highlight
  and rim); `surface_terms(s)` sets them to nothing. The lighting compares the literal, so
  each type's passes hold its model's code only.
- **Where each model enters** (`shade_surface`, `model_light`): the sun and the local lights
  through `model_light` (glTF's lobes for lit, as before); the sky light per model:
  - unlit returns the base color and the emission before any lighting;
  - clear coat: Filament's coat (GGX, Kelemen, IOR 1.5, the interpolated normal), the base
    under it at least as rough and its f0 against the coat; the coat's sky lobe is the one
    the reflections replace (ADR 0073: one lobe per pixel, and the coat's sharp one is what
    shows);
  - cloth: Lambert and Filament's sheen (Charlie, Neubelt); its sky light through an
    analytic fit of the lobe's albedo (`sheen_albedo`, within 0.045 of the integral;
    `docs/research/custom-materials.md`) instead of a third channel in the DFG table,
    which every lit pixel reads;
  - hair: Lambert and two Kajiya-Kay highlights (Scheuermann), the second in the hair's
    color;
  - toon: bands of n·l starting at 0 (where the shadow test cuts the sun), a flat
    highlight where GGX is above half its peak, a rim by n·v.
- **A bug of step 2 fixed:** the depth prepass and the subsurface pass drew material types
  with the types' shading pipelines (a validation error: three color targets in a pass
  with none), found by the first golden shot with a prepass and types. They draw types
  with the engine's pipelines now (`draw_phase`'s `types`).
- **Measured** (ref-1):
  - the built-in pipelines are unchanged: every shader property identical on the Radeon
    (`scene_bench --sky --pipeline-stats`, 17 pipelines);
  - each model's registers, Radeon PS VGPRs without / with local lights / with sky light /
    with both (`material_registers`, a new example): glTF (textured) 48 / 64 / 64 / 80, a
    lit type without textures 34 / 60 / 61 / 85, unlit 5 / 5 / 5 / 5, clear coat
    40 / 67 / 63 / 78, cloth 36 / 50 / 55 / 69, hair 40 / 69 / 54 / 74, toon 47 / 62 / 63 / 81;
    no scratch memory; the six types' 24 pipelines compiled (one compiler thread) and made
    in 1.5–3.1 s;
  - golden shots `procedural_shading_models` (the bottom row of spheres, one model each)
    and `procedural_shading_models_reflections` (on the polished floor with probes and
    screen-space reflections), per vendor, the same bytes on every run.
- **Deterministic shots with types:** a shot's frames must not depend on how long the
  compiles take (the screen-space reflections' noise follows the frame count, and the
  first reflection shot differed between runs). `ForwardRenderer::wait_for_material_types`
  (for tests and tools) blocks on the compiler's results and the warm-up thread; the
  golden harness calls it after every frame, so the first frame draws the fallbacks and
  every later one the types.
- **Not yet:**
  - glTF's `KHR_materials_unlit`, `_clearcoat` and `_sheen` imported onto the models: they
    add fields to the cooked material, so they come with the material file (step 4);
  - Marschner's hair, cloth's subsurface color, anisotropic GGX;
  - the models in the subsurface entry points (subsurface stays glTF's lit).

### Step 4's design (the owner, 2026-10-12)

- **The material file** (`*.material.ron`; the owner confirmed its shape): the type's
  module (`shader`) and name (`type`), its `Params` fields by name (numbers, vectors
  `(x, y, z)`, arrays `[...]`, `Texture("path", Color | Data | Normal)`), the glTF
  `fallback`, `alpha` (`Opaque`, `Mask(cutoff)`, `Blend`) and `double_sided`. Paths are
  relative to the file. An unset field is zero (a texture: none, white) and reported; an
  unknown name or a value of the wrong shape is an error naming the field.
- **Binding to meshes** (the owner's choice): `<scene>.materials.ron` next to the glTF
  scene maps glTF material names to material files; the cook applies it, and the glTF
  material it replaces is the fallback unless the file sets one.
- **The layout** comes from Slang's reflection of `<type>.Params` (fields and types). Its
  layout rules are std140 and std430, but a struct read through a pointer has Slang's
  natural layout (`float3` then `float` in 16 bytes, `float4` aligned to 4), so the
  offsets are the engine's, checked against compiled SPIR-V by a test.
- **Textures** in parameters are `TextureRef` fields (`ctx.sample(p.pattern)`).
- **Sub-steps:** 4a material files and layouts (dev path, golden shots through files);
  4b cooking: the map, textures, pack version 5 with typed materials and precompiled
  SPIR-V; 4c hot reload of type modules and material files; then "Compile All Shaders" and
  PSO records where measurements call for them.

### Step 4a as built (2026-10-12)

- `engine_shader::Compiler::reflect_struct`: the front end only (`-skip-codegen`, 0.12 s
  for `forward.slang` with a type against 7.1 s with code for every entry point), fields
  from `spReflection_FindTypeByName("<type>.Params")`, natural offsets
  (`engine_shader::reflect`). The test compiles a probe of every field kind (`float3`
  packing, `int3`, arrays, nested structs, arrays of structs, `TextureRef`) and compares
  every offset and the size with the SPIR-V's `Offset` and `ArrayStride` decorations.
- `engine_assets::material_file`: `MaterialFile` (RON through `ron` 0.12 with implicit
  `Some`), `build_params`, `MaterialMap` (the scene's map).
- `engine_render::forward::reflect_material_params` and `material_type_options`: the one
  place the types' compile convention lives; registering the same module and type again
  gives the same id.
- The golden shots' types are set by material files now
  (`tests/golden/materials/*.material.ron`), with `ModelParams` regrouped into named
  fields (`float3`s after scalars, not where std430 would put them): every typed shot is
  the same bytes as before on both GPUs.
- The built-in SPIR-V did not change (`TextureRef` and its `sample` are unused there).

### Step 4b as built (2026-10-12)

- **Cooking.** `ofeliaengine cook` reads `<scene>.materials.ron` (or `--materials <MAP>`);
  `engine_assets::material_file::apply_material_map` replaces the named glTF materials
  (`"#14"` names an unnamed one by index: Sponza's are unnamed) by their files: the
  type's layout and compiled entry points from a `MaterialTypeBuilder`
  (`SlangMaterialTypes`: `engine_shader::material_types`, the compile convention moved
  there so the CLI needs no renderer), texture parameters read as images of the scene and
  cooked with their encoding, the glTF record kept as the fallback.
- **Pack version 5** (`pack::VERSION`): a `MaterialType` entry per type (module, type,
  every fragment entry point's SPIR-V), and the scene entry's `meta[4..6]` sections: the
  typed materials (`TypedMaterialRecord`), their texture parameters and parameter bytes.
  Packs without types stay at their old version.
- **Loading.** `engine_streaming::register_material_types` registers a pack's types with
  their SPIR-V (`ForwardRenderer::register_material_type_precompiled`: pipelines are made
  without compiling) and `add_materials` adds the scene's materials, typed ones with their
  parameters and texture references. `flight` uses them.
- **Measured** (ref-1): a type is 92 fragment entry points; compiled in one request
  10.5 s, on 8 threads with a session each 3.3 s. Sponza's pack with one type: +3.5 MB raw
  (about 38 KB of SPIR-V per entry point), +0.5 MB stored. `flight` on that pack: no
  validation errors, no hitches, the type's 5 pipelines made from the pack's SPIR-V.
- **Golden shot `sponza_fabric`**: Sponza's curtains (materials 14 to 19) as cloth
  (`tests/golden/materials/fabric.slang`, their own textures and normal maps as
  `TextureRef` parameters), through a version 5 pack and back, on both GPUs.
- **Not yet:** glTF's `KHR_materials_unlit`, `_clearcoat` and `_sheen` onto the models;
  PSO records to cook only the entry points a game draws (all 92 now); 4c hot reload.

### Step 4c as built (2026-10-13)

- **Types' modules** (`ForwardRenderer::set_material_hot_reload`): the folders of the
  files a type's compiles read are watched (Slang's dependency list: its module,
  `forward.slang`, their includes; `engine_platform::fs_watch::WatchSet`). A change quiet
  for 30 ms (editors save in steps) compiles the entry points the type is drawn with again,
  in one request on the material compiler thread, tagged with a generation (a stale result
  is dropped). Entry points not drawn are forgotten and compile from the new source when
  first drawn (a cooked type's 92 do not all compile again).
- **No gap on screen:** a pipeline in use keeps drawing until its successor is made
  (`Derived::Ready::previous`), then is retired and destroyed once the frames that may
  use it are done (`ForwardRenderer::collect_material_types`, called by the frame
  renderer with the frame number, as bindless indices are).
- **Errors:** a module that does not compile keeps the previous version drawing; the error
  (Slang's diagnostics, at the module's line) is kept until the next good compile
  (`ForwardRenderer::material_type_error`).
- **Material files** (`engine_streaming::material_files::LiveMaterialFiles`): a material
  loaded from a file is laid out again and replaced in the table (`set_typed`) when the
  file changes; its type's layout is cached and reflected again only when the module
  changes; a file that does not parse keeps the previous parameters, its error kept. The
  golden harness loads its material files through it.
- **Measured** (`engine_frame` test `hot_reload`, both GPUs, validation on): a type's
  module edited to on screen 240–266 ms (budget 500 ms, CLAUDE.md §5: the 30 ms quiet, the
  front end and one entry point's code, the pipeline); a material file edited to on screen
  31 ms (the quiet time; no compile). No validation messages as pipelines are replaced.

### Step 5: the ubershader experiment (2026-10-13)

The owner asked to try ubershaders against the plain fallback (B.2 against B.3). The
prototype (branch `exp/ubershader`, not merged): `fragmentUber` and `fragmentUberMasked`
read the view's features (ambient occlusion, local shadows, contact shadows, decals, GI)
from a push constant at run time, local lights and the environment compiled in and skipped
at run time; `ForwardRenderer::set_ubershader` draws the main shading with them, glTF
included, so their cost is measured on Sponza.

| | Specialized | Ubershader |
|---|---|---|
| Radeon PS VGPRs | 48 / 64 / 64 / 80 (the four variants) | 90 |
| RTX forward, sky light | 377 µs | 430 µs (+14 %) |
| RTX forward, prepass, AO, contact shadows, 32 lights | 357 µs | 385 µs (+8 %) |
| Radeon forward, sky light | 18.2 ms | 27.9 ms (+53 %) |
| Radeon forward, prepass, AO, contact shadows, 32 lights | 17.9 ms | 21.8 ms (+22 %) |
| Compile one entry point (glTF or a cloth type) | 205–220 ms | 215–225 ms |
| Picture (1920 × 1080, both GPUs) | | 3 pixels differ, by 1/255 |

**Decision: the glTF fallback stays (B.3); no ubershader.**
- The picture is the same, as the ADR asked.
- The cost fails on the iGPU: its 90 registers cut occupancy, and a type drawn with it
  would cost up to half the frame's forward pass while its specialized pipelines compile.
- Nothing is gained at first sight: compiling the ubershader's one entry point takes as
  long as compiling the specialized one the view needs.
- It would help only a view that changes its features while a type is on screen (one
  compile for every feature set). Cooked packs carry every entry point's SPIR-V, so such
  a change costs a pipeline made on the warm-up thread, not a compile.

To measure again, on new hardware or drivers: `scene_bench --uber` on the branch.

## Consequences

- **The built-in path stays as fast as today** (identical SPIR-V), and the register rule
  holds: material differences are types, not branches.
- **Every pass that draws surfaces becomes generic:** forward shading, depth prepass,
  shadows, motion, feedback, reflections' entry points and the decal/GI variants. This is
  the bulk of the work, done once.
- **The number of pipelines grows with the materials actually drawn** (about one per
  material per feature set in use), not with every possible combination.
- **The pipeline cache and PSO records become part of a game's build.**
- **A material type is code:** it can be slow. The profiler shows its pipelines' GPU time
  per batch, as the passes do now.

## Addendum (2026-10-13): glTF's unlit, clear coat and sheen as built-in types

Step 3's leftover ("glTF's extensions onto them come with the material file"), after
step 4: `KHR_materials_unlit`, `KHR_materials_clearcoat` and `KHR_materials_sheen` imported
onto the shading models. Research: [docs/research/gltf-material-extensions.md](../research/gltf-material-extensions.md)
(Filament's gltfio, three.js, Bevy 0.20, Godot; the Khronos specifications).

**Options for sheen.**

1. **Onto `MODEL_CLOTH`.** No new model, but cloth drops GGX: glTF's sheen is a layer over
   the metallic-roughness base, highlight kept (`material · (1 − max3(sheen) · E(N·V)) +
   sheen`), and Filament's importer keeps its cloth model out of glTF too.
2. **A new model, lit under the sheen layer (chosen):** `MODEL_SHEEN`, and
   `MODEL_SHEEN_CLEARCOAT` for both extensions (the coat over the sheen, as the
   specification layers them). E is the Charlie and Neubelt lobe's albedo fit cloth
   already has (`sheen_albedo`), as Filament's `DFG_cloth` matches its lobe.

**As built.**

- **Import** (`engine_assets`, `gltf` with its `extensions` and `KHR_materials_unlit`
  features: flags only, no new crates):
  - `MaterialData::layers` (`MaterialLayers`): unlit, `ClearcoatData`, `SheenData`;
  - a zero coat factor or a black sheen turns the layer off, and unlit excludes the others;
  - clear coat and sheen are read from their JSON. Files may list them in
    `extensionsRequired`: they are taken out of that list before the crate's validation,
    which still refuses unknown required extensions.
- **Types** (`shaders/materials/gltf.slang`, a module without entry points, which the shader
  build skips):
  - `GltfUnlit`, `GltfClearcoat`, `GltfSheen` and `GltfSheenClearcoat`;
  - each reads its glTF record from the material table, as `GltfMaterial` does (and reuses
    its functions), and its extension's parameters from its slot (`material_params`), with
    a copy of the record's alpha cutoff for the masked depth passes' `alpha_cutoff`.
- **`engine_assets::gltf_models::apply_gltf_models`:**
  - turns those materials into typed ones, after the material files (a file wins);
  - builds each type once with the `MaterialTypeBuilder` (Slang: the 128 fragment entry
    points in 3.3–4.1 s; the layout through reflection, every field checked against the
    values written);
  - the cook (`ofeliaengine cook`) and the golden harness call it. Packs need no new
    format: the types and their SPIR-V go in as material files' types do (version 5).
- **The models** (`forward.slang`):
  - the coat's own normal map (`Surface.clearcoat_normal`; none: the interpolated normal,
    as before);
  - the emission under the coat darkened by `1 − clearcoat · F`, as the specification says;
  - the sheen layer's scaling computed once per pixel (`ModelTerms.sheen_scaling`);
  - its sky light: the base's ambient and the reflections' weight scaled by it, plus the
    sheen's lobe in the sky.
- **Measured** (ref-1):
  - the built-in pipelines are unchanged: every shader property identical on the Radeon
    (`scene_bench --sky --ao --pipeline-stats`, 25 pipelines). The SPIR-V differs only in
    the order of constants;
  - the test types' registers are identical to main (`material_registers`);
  - the new types, Radeon PS VGPRs without / with local lights / with sky light / with
    both: unlit 11 / 11 / 11 / 11, clear coat 59 / 70 / 82 / 96, sheen 48 / 65 / 79 / 84,
    sheen and clear coat 64 / 80 / 87 / 100 (glTF's textured base is 48 / 64 / 64 / 80); no
    scratch memory;
  - golden shots `gltf_clearcoat` (ClearCoatTest), `gltf_clearcoat_wicker`, `gltf_sheen`
    (SheenTestGrid) and `gltf_unlit` per vendor, the same bytes on every run, compared by
    eye with the Khronos sample viewer's screenshots.
- **Not yet:**
  - texture coordinates other than the first set, and `KHR_texture_transform` (as for
    glTF's other textures; SheenChair needs both);
  - vertex colors (for unlit too);
  - PSO records, so that a cook compiles only the entry points a game draws.

## Addendum (2026-10-14): PSO records

The decision's "Compile All Shaders over the pairs a project's PSO collection recorded".
Research: [docs/research/pso-records.md](../research/pso-records.md) (Unreal's bundled PSO
cache, Unity 6's PSO tracing, Godot 4.4's precompilation).

**Options.**

1. **Records from playtests (chosen):** what frames drew, merged across runs (Unreal,
   Unity).
2. **Prediction:** the entry points of the feature sets each device class turns on (Godot,
   Unreal's precaching). It needs no playtest, but it misses what a game turns on itself.
   It stays a candidate for types no record names.

**As built.**

- **What is recorded:** every (type, fragment entry point) pair frames asked for
  (`ForwardRenderer::material_type_entries_drawn`), the bent-normal twins' included.
  First sight counts: a type drawn under ambient occlusion records its plain entry point
  (its first frames) and the twin's.
- **The record** (`engine_assets::pso_record::PsoRecord`, RON, version 1):
  - by "module file name:type", sorted, so a team shares it and merges diff cleanly;
  - saved by merging into the file under a file lock (`save_merged`): parallel runs keep
    each other's entries;
  - written by `flight --record-pso <file>` and by the golden harness
    (`ENGINE_GOLDEN_PSO_RECORD=<file>`).
- **The cook:**
  - `ofeliaengine cook --pso <record>` (or `<scene>.pso.ron`) compiles only the record's
    entry points for the types it names, through
    `SlangMaterialTypes::with_pso_record` and
    `engine_shader::material_types::compile_material_type_entries`;
  - a type the record does not name is cooked whole;
  - a pack's type lacking an entry point frames ask for logs a warning and is counted
    (`ForwardRenderer::material_type_pack_misses`). Where Slang is there, the entry point
    then compiles on first sight; in a shipped game the type draws without it.
- **Degrading within the type:** a type without its bent-normal twin's pipeline (compiling,
  or missing from its pack) draws its plain variant, not its glTF fallback
  (`typed_pipelines_or`). Checked by hand: the twin left out of the record and run-time
  compiles disabled, ClearCoatTest under AO kept its coats (FLIP 0.006 against the bent
  image, the difference only in ambient light).
- **The golden shots cook with their own record** (`tests/golden/pso.ron`, recorded on both
  GPUs) and fail on any miss, with the command to record it again. A shot that starts
  drawing a new variant makes the record stale at once.

**Measured** (ref-1):

| Scene (one type) | Compile, all → recorded | Pack |
|---|---|---|
| ClearCoatTest | 128 entry points 4.0 s → 1 in 0.6 s | 1.93 → 0.98 MB |
| SheenTestGrid | 3.8 s → 0.6 s | 0.97 → 0.09 MB |
| UnlitTest | 3.2 s → 0.5 s | 127 → 20 KB |

The golden suite (one thread) took 105 s with every entry point and 78 s with the record
(−26 %), on both GPUs.

**Consequences.**

- A game's build includes its PSO record. A view feature the playtests never turned on
  draws its types' fallbacks (logged), as Unreal's misses hitch: record with every quality
  preset a game offers.
- Not yet:
  - the warm-up making recorded pipelines first at load (ADR 0079's next step);
  - prediction for types no record names;
  - "Compile All Shaders" in the editor (there is no editor yet).
