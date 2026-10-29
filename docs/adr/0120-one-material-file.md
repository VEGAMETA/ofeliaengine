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

## As built (E10-1b, 2026-10-28): typed materials drawn by the player

- **Their layout off the frame:** the forward renderer's material compiler thread also
  reflects a type's parameters' layout (`ForwardRenderer::material_params`): asked for the
  first time it returns nothing and sends the reflection with the next
  `update_material_types`; then the layout, or why it failed, and a revision. A reload of
  the type's module reflects it again with its entry points and moves the revision, the
  previous layout kept until then (and when the new one fails). Reflections count as
  pending material types.
- **The player** (`engine_app::Assets`) draws a typed `.material` on a `Model` with its look
  at once. `Assets::make_material_types`, once a frame where the forward renderer is at
  hand:
  - registers the type;
  - lays the parameters out once the layout comes, and again when the revision moves;
  - asks for its texture parameters' images (`loading::Job::Image`, decoded or loaded
    compressed from the derived data cache as glTF textures are);
  - gives the material its parameters as the images are made.

  A type that fails draws the look, and the file is an asset problem.
- **Read again** (E4-5): the previous version draws until the new one is laid out and has
  its images. An image written anew reads the material files that use it again.
- **Previews** (the editor's thumbnails): one with a material type is drawn once to ask for
  its pipelines and not answered. It is drawn again, and answered, once no material type's
  pipeline is on its way.
- **The editor** turns the types' hot reload on (`Player::set_material_hot_reload`).
- **Measured** (ref-1, dev build, 320 × 180 offscreen):
  - from load to drawn with its type: 1.62 s on the RTX, 1.05 s on the Radeon;
  - the longest frame meanwhile: 7.6 ms on the RTX, 5.7 ms on the Radeon (nothing compiles
    on the frame's thread);
  - a file written anew drawn in 4 ms (the layout reflected already).
- **Tests:** a cube draws its typed file's look first (green), then its type (red) on both
  GPUs. A fabric's texture parameter gets its image. The type's preview shows the type. The
  file written anew (blue) replaces the red only once laid out.
- **Not done:**
  - the inspector's parameter rows, New Material, Make Variant and Apply to Selection
    (E10-1c);
  - an image written anew keeps its previous texture until it is written again;
  - a material type's preview waits for every material type's pipelines, not only its own.

## As built (E10-1c-1, 2026-10-28): a material file edited in the inspector

- **Picked in the dock**, a `.material` shows a Material section under its path and kind
  (`engine_editor::material`):
  - a standard file: base color, metallic, roughness, emissive, alpha, its cutoff when
    masked, double sided;
  - a typed file: the type and its module; a row per field of the type's `Params` layout,
    which the player reflects off the frame (`Player::material_type`,
    `Player::material_params`): numbers, a vector's parts (r g b a when the field's name
    says color, tint, albedo or base, until Slang attributes give hints in E10-2), an image
    with its encoding, an array shown with its count; then the look and its values.
- **The editor holds the file** (`MaterialDoc`: the file, the paths its references were
  written with, its text now and as saved) and gives it to the player whenever it changes
  (`Player::set_material_file`, `Assets::set_material`):
  - the held file is read in place of the file on disk;
  - its material is made again in its place: a standard file's values at once, a typed
    file's parameters laid out again from the cached layout with no frame of its look.

  An unedited file written anew on disk is read again; an edited one keeps its edits.
- **Edits are steps of the one history:** `History::change_file` records a file's text
  before and after a step, and drags merge as entity drags do. Undo, redo and a drag's
  Escape hand the texts back (`History::take_files`) to the held files.
- **Ctrl+S** writes the edited material files with the scene. The section says "Material
  (unsaved: Ctrl+S)" until then.
- **Tests:**
  - the history: a file's text merged over a drag, handed back by undo, redo and cancel;
  - the rows of a standard and a typed file (a vector to `vectors`, a number to `numbers`,
    a whole number rounded, an image with its encoding, the look replaced hiding its
    values);
  - the editor: a click on a material's tile, roughness typed, undone and redone, a mask
    bringing its cutoff's row, Ctrl+S writing the file;
  - the player, on both GPUs: a held typed file laid out again in the same material,
    never its look in between.
- **Not done:**
  - editing from a selected object's `Model.material` (E10-1c-2), New Material, Make
    Variant and Apply to Selection;
  - unsaved material edits are not in the session's autosave yet;
  - edits made during Play stay, but their steps go with Play's;
  - image previews on image rows.

## As built (E10-1c-2, 2026-10-28): from the object, the material commands, autosaves

- **From the object:** the material shown is the file picked in the dock, else the selected
  entity's `Model.material` when it is a material file. Its rows come after the entity's
  components, and the section's title counts the scene's models that use it ("Material ·
  used by 3 objects"). Edits there are the material's, as from the dock.
- **The palette's Material commands:**
  - **New Material:** `New Material.material` (a number after it when taken) in the dock's
    folder, a standard material with an id of its own and its `.meta`. It is given to the
    selected models in one step, else picked in the dock.
  - **Make Variant:** the material shown copied beside its file (`Red 2.material`), its
    unsaved edits with it, with an id of its own, given to the selected models in one step.
    The original stays as it is (the UX note's "a copy first", variants by `base` in E10-4).
  - **Apply to Selection:** the material picked in the dock given to the selected models.
  - Giving is `Model.material` set on every selected model as one undo step; the files are
    made on disk, not undone.
- **Unsaved edits in the session,** as scripts' are: a dirty material's text goes to
  `.ofelia/autosave/<stem>-<hash>.material` once the edits are quiet (and at once before
  Play, a restart and the end). The session lists it in a `[material]` block, and a new
  editor holds the file again with its edits, unsaved. Saving deletes the autosave.
- **Tests:**
  - New Material given to the selected box, its rows and its users under the box, an edit
    there, Make Variant with the edit given to the box and the original kept, undo putting
    the original back, Apply to Selection;
  - a material's unsaved edit back in a new editor's session, the file untouched until
    Ctrl+S, the autosave gone after it;
  - the session's `[material]` block read back.
- **Not done:**
  - the material's thumbnail before its edits are saved (it is drawn from the file);
  - material slots as asset fields (ADR 0115's last item);
  - variants that follow their base (`base`, E10-4).

## Addendum (E10-4a, 2026-10-29): variants that follow their base

The team answered the materials UX note's second question with b, in E10-4 (2026-10-28).
A material file may name its `base` and hold only what it changes, and the inspector marks
those values as it marks a prefab's overrides. Until now Make Variant copied the file
("a copy first").

### The file

```text
[material version=2 id=9a8b7c6d5e4f30211203f4e5d6c7b8a9]

base = asset("materials/red.material", 4f8e1d2c3b4a59687706f5e4d3c2b1a0)
base_color = [0.1, 0.3, 0.9, 1]
numbers["roughness"] = 0.2
```

- **`base`** names another material file. Without one (the default, `none`, not written)
  the file is as it was before.
- **What it holds** is only what it changes: every line its base's resolved file does not
  have, the `base` line included. A value set to what the base has is not written, so a
  later change to the base reaches the variant.
- **Maps merge by key.** A variant that sets `numbers["roughness"]` keeps the base's other
  numbers. A variant cannot remove a key its base sets; no case for that has come up.
- **The version stays 2.** A file without `base` is unchanged, and the editor and the
  engine are one build, so no older reader meets the new field.

### Options weighed

- **a. Lines over the base's value (chosen).**
  - It uses one reader and the scene grammar's own rule: a line sets one field and leaves
    the rest.
  - A variant's text is a diff of lines, so it merges in version control the way a prefab
    instance's overrides do.
- **b. A list of overridden field names next to full values,** as Unity's material variants
  keep overrides as property paths.
  - The list has to be kept in step with the values.
  - Every value is still written, so a change to the base does not reach the variant
    without the list. It gains nothing over a.
- **c. Flatten on save:** the variant is written whole and its base is only a note.
  - An edit of the base would not reach its variants, which is what the team chose
    against.

### Decision

- **Reading** (`MaterialFile::read_with_bases(text, root, open)`, `load_with_bases`):
  - while a file names a base, the base is read first, then the variant's own lines over
    the base's values (`engine_scene::read_value` keeps what the text does not name);
  - a base comes from `open` when the editor holds it, unsaved edits and all; otherwise it
    is read from its file through its path in the variant;
  - the result (`ResolvedMaterial`) is the resolved file, the report with every path in
    the chain (the variant's own over its bases'), the bases (id, and file when read from
    disk), and the base resolved without the variant's lines.
  - `read_over(text, base)` reads a variant's text over a base already resolved: what the
    editor does for undo and when a base changes.
- **Refused, with the file's path:**
  - a base that is the file itself, or that comes back to any file in the chain, as
    prefabs refuse cycles;
  - a base with no path written;
  - a base file that holds another material's id.
- **Writing** (`to_variant_text(id, assets, base)`): the header, then each of the file's
  lines that the base's text does not have, both written with the same paths.
- **Every reader resolves:**
  - the player's assets;
  - the cook (`apply_material_map`);
  - the live material files of a cooked game in development (`engine_streaming`);
  - the editor's inspector.

  The cook writes materials out resolved, so a pack holds no chains and a game never
  resolves.

### As built (E10-4a, 2026-10-29)

- **The player** (`engine_app::assets`):
  - `read_material` reads a material over its bases, a base the editor holds coming from
    there, and gives back the bases it read;
  - `note_bases` makes each base file one of the variant's dependencies, so the
    variant's material is made again when a base's file is written anew (`changed`, as an
    image's user is);
  - it also records the variant under each of its bases, so when the editor holds a new
    version of a base (`set_material`), that base and every variant recorded under it are
    made again in place (`remake_material`).
- **Live material files** watch a variant's base files with its own and its module, and
  read the variant again when one changes.
- **The editor:**
  - a material's doc (`MaterialDoc`) keeps a variant's base resolved. It shows the base's
    values under the variant's own, writes only what the variant changes, and reads an
    undone text over the same base;
  - a base the inspector holds is followed every frame (compared, copied only when it
    differs). A base the inspector does not hold is read again when its file changes. In
    both cases the variant's own values are kept and the variant goes to the player
    again;
  - **Make Variant** now writes a variant: a file of `base` alone, given to the selection
    in one step. It draws as its base, the base's unsaved edits with it.
- **Tests:**
  - in `engine_assets`:
    - a variant read over its base's file, and over a base held in memory;
    - a variant of a variant;
    - the text holding only what the variant changed, read back the same;
    - a base's later edit reaching the variant;
    - a chain that goes round, and a base without a path, refused;
  - in the editor:
    - a variant's doc writing only its own lines;
    - the doc following its base with its own values kept;
    - undo reading over the base;
    - Make Variant writing `base` alone and showing the base's unsaved edit;
  - on the GPU (the player): a variant drawn in its base's colour, then following its base's
    file written anew (3 ms after the change, on the Radeon), then following its base held
    by the editor (2 ms).
- **Next (E10-4b):** the inspector marks the values a variant sets as a prefab's overrides
  are marked (a bold label and a bar). ↺ puts one back to its base's value, Apply to Base
  writes it into the base, and a Base row names the base and opens it.

### As built (E10-4b, 2026-10-29): a variant in the inspector

- **Its own values:** a row of a variant is changed where it is not its base's (a
  parameter by its entry in the base's maps, a texture's encoding by the base's), so ↺
  shows on the variant's own values only, as on a prefab instance's overrides, and puts the
  base's value back (`material::changed` and `reset` take the base). The inspector shows
  overrides by ↺ alone, for prefabs too; the bold label and bar the UX note proposed stay a
  later polish of both.
- **A Base row** first (`Row::Base`, the base's file name, a material's icon): a click
  shows the base in the FileSystem dock. The section's title says whose variant it is
  ("Material · variant of red.material · used by 2 objects").
- **Apply to Base** (the palette's Material commands, as a prefab instance's Apply
  Overrides): the base (held for the inspector, opened if it was not) takes the variant's
  values, its own base kept; the variant, over its base as it now is, keeps its `base` line
  alone. One step changing both files; undo takes both back, and the variant follows its
  base again in the editor's frame (the following moved from the player's sync to
  `Editor::update`, so the editor's model holds without a player).
- **Tests:** a variant's rows against its base (its base row first; changed where its own,
  reset to the base's for a number and a texture); in the editor, the title, ↺ back to the
  base's value with the variant's line gone, Apply to Base as one step (the base takes the
  value, the variant names its base alone) and its undo.

### As built (E10-4c, 2026-10-29): what a material costs

The materials UX note's item 12 (cost from measurements, registers first). Options weighed
for where the numbers come from:

- **a. The driver's statistics of every pipeline the editor makes (chosen).** The device
  captures them (`VK_KHR_pipeline_executable_properties`) when the editor asks
  (`PlayerSettings::pipeline_statistics`, `ExtraQueues::pipeline_statistics`; games do
  not). Measured on ref-1 (2026-10-29, `material_registers` over a salted copy of the six
  shading models so the drivers' caches miss): cold material pipelines took 532 ms with
  the capture against 542 ms without on the Radeon, 758 against 768 ms on the RTX 4060 Ti,
  within the runs' spread; pipelines from the cache keep their statistics.
- **b. A second pipeline of the inspected type, made with the capture on demand.** One more
  compile per inspected type, and two code paths for one number: no gain over a.

What the editor shows:

- `ForwardRenderer::material_type_cost(device, ty)` (`Player::material_cost`) reads the
  type's main shading per variant (plain, local lights, environment, both): its fragment
  stage's registers (AMD's `numUsedVgprs`, NVIDIA's `Register Count`, the perf gate's
  watched names) and scratch, beside the engine's own shading's where made. It asks for the
  variants (made off the frame's thread) and answers once no material type is being made;
  a wait reads nothing and allocates nothing.
- The material inspector's **cost** row after the type: `35–40 registers (the engine's
  47–56), 128 B spilled · 48 B of parameters` (the parameters' size from the layout), read
  once per layout revision.
- A graph's bar: the same of its preview's type once a drawing of new code comes back,
  marked as the editing build's (its numbers parameters, ADR 0123's answer 3a).
- Tests: the statistics' names of both vendors read (the vertex stage's and AMD's
  available VGPRs not); the cost row's text; on the GPU, the stripes type's cost after it
  draws (39 VGPRs against the engine's 47 on the Radeon, 40 registers against 56 on the
  RTX; no spills).
- Not yet: SPIR-V instructions and image samples, compile time, Draw as Fallback.

### As built (E10-4d, 2026-10-29): the material's preview at the top of its section

The materials UX note's item 11: the engine's own preview (ADR 0112), orbited by dragging.

- **What it shows:** the shown material on the selected object's model's mesh when the
  material is that model's, else on a sphere, through the player's previews (its own key,
  `PREVIEW_SIZE` square) into a texture of its own under the section's header, "Drawing…"
  until the first picture comes back.
- **When it draws:** when its material doc changes (its values alone, the same shader and
  type: drawn once, `same_code`); while it is dragged, at most one drawing a frame, the
  turn about the vertical (sideways) and the sideways axis (up and down, a quarter turn each
  way at most). A steady frame asks nothing and allocates nothing.
- **Tests:** the preview on the box's own mesh, asked again after an edit (values alone),
  turned by a drag, gone with nothing shown; on the GPU, the red material drawn in the
  inspector's column (7322 pixels, 0.04–0.05 s on both GPUs) while the box is far out of
  view.
- Not yet: a model dropped on the preview, its own light and turn kept per material, a
  larger picture.

### As built (E10-4e, 2026-10-29): a material shown on an object while dragged over it

The materials UX note's item 11, its last part (E4-3's left item): what a drop would do,
seen before it.

- **The try:** a material dragged from the dock over an object in the view (the one the
  hover pick names) or over its row in the tree is put in the object's `Model.material`
  at once, in no undo step; the drag keeps the object's own. Another object, nothing under
  the pointer, leaving the view, Escape, Play or the scene let go give the own back. On
  the release it is given back first, so that the drop records own → new as its one undo
  step. An object that has the material already is left as it is.
- **The pick stays on the object:** a masked or blended material on it would let the next
  hover pick through it to what is behind, the try move there, the object's own come back,
  the next pick find it again: on and off every four frames on the Radeon with a cut-away
  material and the pointer moving (seen with the slot left out). So the scene pick is told
  the object's instance (`Picker::add_scene_pass`'s `solid`, ADR 0109's as-built) and
  draws it as opaque, without its mask; the editor knows the instance from the hover
  pick's sync, the frame the material shows.
- **Tests:** the try, the move to another object, off it, onto a row and Escape, all in
  no step, then a drop as one step that undo takes back to the own; on the GPU, on both
  GPUs, the box red in the view while the red material is over it and its own drawn once
  the pointer is off it, then a cut-away material kept on the box over 60 frames of a
  moving pointer; the scene pick's solid slots under the validation layer.
- Not yet: a try over an inspector field (every selected object), a model's further
  material slots (the drop gives `Model.material`, as before).
