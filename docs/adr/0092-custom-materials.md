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
