# 0120. One material file

- Status: **Accepted (team decision, 2026-10-28):** option a of the materials UX note's first
  question (`docs/research/ux/materials.md`), with the other three answered the same day by
  their recommendations.
- Date: 2026-10-28
- Amends: [ADR 0092](0092-custom-materials.md) (material files, `*.material.ron`) and
  [ADR 0108](0108-scene-format-2.md) (the editor's `.material`). Starts
  [ADR 0107](0107-editor.md)'s E10-1.

## Context

Two files describe materials:

- **`*.material.ron`** (ADR 0092, confirmed by the team on 2026-10-12): a material type's
  instance. It names a Slang module and a type implementing `IMaterial`, sets the type's
  `Params` fields by name (numbers, vectors, arrays, textures with their encoding), and gives
  the look the material has before its shaders are made and in GI's rays (`fallback`), its
  alpha and whether it is double sided. The cook reads it, through a scene's
  `<scene>.materials.ron` map (glTF material name to file), and so does the run time
  (`engine_streaming::LiveMaterialFiles`, kept live in development). Paths are relative to
  the file's folder.
- **`.material`** (ADR 0108, E1-2): the editor's file, in the editor's grammar with its id
  in its header. It holds only the standard material's values (base color, metallic,
  roughness, emissive, alpha, double sided). The player draws it; the editor edits it.

The editor cannot open the first, and the cook cannot read the second. E10 (materials in the
editor) needs one file both read. The UX note asked which:

- **a.** `.material` grows to hold everything `.material.ron` holds; the cook reads it; the
  six golden files are converted and `.material.ron` is retired;
- **b.** the editor reads and writes `.material.ron`, its id in a `.meta` (a second grammar
  in the editor's save path, comments and order kept by hand, a `.meta` beside our own file);
- **c.** both stay (two readers, two reload paths, a choice for every developer).

The team chose **a** (2026-10-28).

## Decision

### The file: `.material`, version 2

```text
[material version=2 id=4f8e1d2c3b4a59687706f5e4d3c2b1a0]

shader = asset("materials/fabric.slang", 7d1c0e9a2b3f4c5d8e6f7a8b9c0d1e2f)
type = "FabricMaterial"
numbers["roughness"] = 0.9
numbers["sheen_roughness"] = 0.5
vectors["tint"] = [1, 1, 1, 1]
vectors["sheen"] = [0.1, 0.2, 0.1]
textures["albedo"].image = asset("textures/curtain.jpg", 0a1b2c3d4e5f60718293a4b5c6d7e8f9)
textures["albedo"].encoding = Color
textures["normal"].image = asset("textures/curtain_n.jpg", 1b2c3d4e5f60718293a4b5c6d7e8f90a)
textures["normal"].encoding = Normal
look = Own
base_color = [0.8, 0.8, 0.8, 1]
metallic = 0
roughness = 0.5
emissive = [0, 0, 0]
alpha = Opaque
alpha_cutoff = 0.5
double_sided = false
```

- **`shader` and `type`:** the module and the type implementing `IMaterial`. With no shader
  (`none`, the default) the file is the engine's standard material, as version 1 was, and
  the parameters below it are not used.
- **The type's parameters** by their `Params` field's name (`outer.inner` for a nested
  struct's field, `items[2].inner` for an array of structs', as before):
  - `numbers`: a field of one number (a `float`, an `int`, a `uint`);
  - `vectors`: a vector's values, or an array's in order (an array of vectors written flat:
    `float3 points[2]` takes six numbers);
  - `textures`: a `TextureRef` field's image, an asset reference, and its encoding (`Color`,
    `Data` or `Normal`: how the cook compresses it).
  - A field the file does not set is zero (a texture none, white), as before.
- **The look** (`look`): what a typed material looks like before its shaders are made, and
  what GI's rays see.
  - `Own` (the default): the standard material's values below (`base_color`, `metallic`,
    `roughness`, `emissive`).
  - `Replaced`: the glTF material the file replaces in a scene's map (the old
    `fallback: None`); where it replaces none, the values below.
  - For the standard material (no shader) the values below are the material.
- **Alpha** (`alpha`, `alpha_cutoff`) and **`double_sided`** as in version 1, for both.
- **Paths are asset references** (ADR 0108): a path from the project's root and the file's
  id. The cook and the run time go by the path the reference was written with, from the
  root they are given; an absolute path (a file outside any project, a test's temporary
  file) is used as it is. The editor follows the id to the file's current path, so renames
  and moves (E4-7) follow, as in scenes. A file outside the project gets an id like any
  other: two references of one id would be one file.
- **Version 1 still reads:** a version 1 file is a standard material with no parameters;
  saving writes version 2.

### Who reads it

- **`engine_assets::MaterialFile`** is the file (reflected, read and written with
  `engine_scene`'s grammar); `engine_app::MaterialFile` is it, re-exported.
- **`MaterialFile::typed(root)`** resolves a typed file into what the cook and the run time
  build parameters from (`TypedMaterial`, the old `.material.ron` reading's shape): the
  module's path, the type, the parameters, the look. `TypedMaterial::build_params` lays them
  out by the type's reflection as before, now also taking an array of vectors written flat.
- **The run time** (`LiveMaterialFiles::load`) reads `.material` files, with the project's
  root.
- **The cook** reads them through the scene's map, `<scene>.materials.ron`, which stays as it
  is (a map of glTF material names to files, relative to the map's folder) and now names
  `.material` files. `ofeliaengine cook` takes the project's root (`--project`, the current
  folder by default). The map goes when scenes name their materials by slot only
  (ADR 0115), not before.
- **`.material.ron` is retired:** no reader is left. The six golden files
  (`tests/golden/materials/`) are converted once, by hand-checked text; the fabric test
  writes version 2. No game has such files yet, so no converter ships.

## Consequences

- One grammar for every file the editor writes, with the id inside the file (ADR 0108).
- `engine_assets` depends on `engine_scene` and `engine_reflect` (no cycle: neither depends
  on it).
- The player and the editor draw a typed material's look until E10-1b registers its type
  and compiles its entry points off the frame (the UX note's E10-1).
- The inspector shows the new fields as plain values until E10-1c gives a typed material's
  parameters their own rows (from its type's reflection: ranges, colors, textures).
- Golden images do not move: the same parameters are laid out the same way.

## As built (E10-1a, 2026-10-28)

- **`engine_assets::material_file::MaterialFile`** is the file: reflected, read and written
  with `engine_scene`'s grammar (`parse`, `load`, `to_text`, `to_text_with` for the
  references' paths), version 2, version 1 read as the standard material.
  `engine_app::MaterialFile` and `Alpha` are it, re-exported; the player draws a file with
  `material_desc` (its standard values: a typed one's look until E10-1b).
- **`MaterialFile::typed(paths, root)`** gives the cook and the run time a
  `TypedMaterial` (the old `.material.ron` reading's shape, renamed): the shader's and the
  textures' paths as the file wrote them, from the root (an absolute one as it is); a field
  set as a number and a vector is an error. `TypedMaterial::build_params` also takes an
  array of vectors written flat.
- **`LiveMaterialFiles::new(shader_dir, root)`** reads typed `.material` files; a standard
  one is refused with a message.
- **`apply_material_map(scene, map, root, builder)`**: a typed file as before; a standard one
  sets the glTF material's values. `ofeliaengine cook --project <DIR>` (the current folder by
  default).
- **The six golden files** are `tests/golden/materials/*.material`, converted by a one-off
  script: their numbers and vectors, each with a fresh id, and `look = Replaced` (none set a
  fallback, so each kept the look of the glTF material it replaces; without the line a
  typed file is `Own`, and the shot of a type's first sight caught it). No texture or alpha
  was set in any. The golden test bed is their project. The fabric shot writes version 2
  files outside any project (absolute paths, fresh ids, `look = Replaced`), and the frame
  crate's hot reload test writes version 2.
- **Tests:** a typed file read, resolved, laid out (an array of vectors written flat) and
  written back the same with its paths; version 1 read as the standard material; an
  absolute shader path kept and `look = Replaced` giving no fallback; errors naming the
  field (an unknown name, a shape, a texture field given a number, a uint, an array, a field
  set twice, a bad line); the map's paths. Golden images unchanged (the gate's GPU step on
  both GPUs).
- **Not done:** the editor's inspector shows a typed file's maps as plain values (E10-1c);
  the player draws a typed material's look (E10-1b); `<scene>.materials.ron` stays RON until
  scenes name materials by slot only (ADR 0115).
