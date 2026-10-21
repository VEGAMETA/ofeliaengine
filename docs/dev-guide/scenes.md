# Scenes

Scenes are text files meant for version control. Two people can change the same
scene and merge like code.

```text
[scene version=3 id=9b41d07c2e8a4f6b8d3c1a5e7f902b64]

[entity 6c1f0a3e9d2b47c88e51f0d4a7b3c912]
order = 1024
Name = "Door"
Transform.translation = (10, 0, 0)
Health = 80

[entity 0b12e4f8c3a94d5e9f7a6b1c2d3e4f50]
parent = 6c1f0a3e9d2b47c88e51f0d4a7b3c912
Name = "Handle"
Transform.translation = (1, -0.5, 0)
Look.mesh = asset("props/door.gltf#Handle", 5f2c0a3e9d2b47c88e51f0d4a7b3c912)
```

## Saving and loading

```rust
use engine_scene::{SaveOptions, SceneId, SceneRegistry, load, save_with};

let mut registry = SceneRegistry::with_engine_types(); // Transform, Name
registry.register::<Health>();                          // your components

let report = load(&mut world, &registry, &std::fs::read_to_string("level.scene")?)?;
for (line, warning) in &report.warnings {
    eprintln!("level.scene:{line}: {warning}");
}

// Save it back with its own id and the paths its asset references were written with.
let mut text = String::new();
let options = SaveOptions { id: report.id, assets: Some(&report.assets) };
save_with(&mut world, &registry, &options, &mut text)?;
std::fs::write("level.scene", &text)?;
```

- Only entities with a `SceneId` are saved. Give new entities one with `SceneId::new()`,
  or call `assign_ids(&mut world)` before saving. Entities without an id are runtime
  objects: bullets, effects.
- A component is saved if it is registered and derives `Reflect`. It must also have a
  `Default`, which provides the values of fields the file does not mention.
- `save` is `save_with` with no id and no asset paths.

## What a file holds

One line per value: `Component.path = value`.

| Value | Written as |
|---|---|
| numbers, booleans, enum variants | `80`, `0.5`, `true`, `Blue` |
| strings | `"Door"`, with `\"`, `\\`, `\n`, `\t`, `\u{…}` |
| vectors and quaternions | `(1, -0.5, 0)` |
| lists | one line per item (`Inventory.items.0.id = 3`), or `[a, b]` when the items are single values; `[]` when empty |
| maps | one line per key: `Inventory.flags["met the smith"] = true`; `{}` when empty |
| optional values | the value, or `none` |
| other entities | `@<scene id>` or `@none` |
| assets | `asset("path#part", <file id>)` or `none` |

- **A component that wraps one value** (`struct Health(f32)`) is written as that value:
  `Health = 80`. So is `Name`.
- **A component with no fields** is written as `Marker = {}`.
- **The header** carries the format's version and the scene's own id.
- **A block's first lines** place the entity: `parent = <id>` for a child (written after
  its parent, in the parent's child order), `order = <n>` for a root (its place among the
  roots, as the editor's tree shows them).

## Names

`Name` is the name the editor's tree shows. Names need not be unique: scene ids identify
entities.

```rust
world.spawn((SceneId::new(), Name::new("Door"), Transform::IDENTITY));
```

## Assets

An `AssetRef` points at a file, or at a part inside one (a mesh or material of a glTF):

```rust
let door = AssetId::parse("5f2c0a3e9d2b47c88e51f0d4a7b3c912").unwrap();
let handle = AssetRef::part(door, "Handle");
```

- **The file's id is the truth;** the path beside it keeps the file readable.
- **Renaming a file in the editor** keeps its id, so references never break (ADR 0108).
- **The paths** come from `LoadReport::assets` or the editor's asset database. Without
  one, a reference is written with the bare id and the part's hash (`#~…`); it still loads
  back to the same asset.

### The asset database

Every asset file under the game's folder has an id (`engine_scene::AssetDatabase`):
- **Our own files carry it in their header:** scenes (`[scene version=3 id=…]`) and
  material files.
- **Foreign files** (glTF, images, sounds, videos, fonts, scripts) carry it in a `.meta`
  beside them:

  ```text
  [asset version=1 id=5f2c0a3e9d2b47c88e51f0d4a7b3c912]
  ```

  The editor writes one the first time it meets a file without one, and so does
  `ofeliaengine assets` (`--json` for tools). Commit them with their files.
- **Moving files:**
  - a file moved outside the editor with its `.meta` keeps its id;
  - one moved without it, its old `.meta` deleted, gets its id back by its size and
    content, for files whose `.meta` the database wrote;
  - otherwise it gets a new id, and references to the old one show it missing, with
    their paths.
- **A copy with its `.meta`** gets a new id for the copy. A scene copied outside the
  editor is reported: save it from the editor as a new file.
- **The index** of ids and paths is a cache in `.ofelia/assets.index`, rebuilt from the
  files at any time. The editor keeps it up to date while it runs: it watches the folder
  and scans a change once the folder has been quiet for 0.15 s. Folders whose name starts
  with `.` and `target/` are left out.
- **Cost:** 10,000 files take 20 ms to scan when unchanged, and 3 ms to load from the
  index. The first scan writes their `.meta` files: 1.3 s.

## In the player

`PlayerSettings::scene` loads a scene file when the player starts, and
`Player::load_scene(path)` loads one later ([player.md](player.md#a-scene-file)):

- **Every reflected component** of the program loads without registration
  (`SceneRegistry::from_program()`).
- **Types marked `hidden`** stay out of scenes: runtime data such as `MeshRenderer` (its
  mesh and material ids live only in memory) and the hierarchy's links.
- **`Disabled = {}`** turns an entity and everything below it off (the editor's eye): not
  drawn, its lights dark, its brushes out of their group. The engine keeps `Inactive` on
  each entity that is off, a tick after the change; your systems can check it
  (`engine_runtime::is_inactive`).

### Models

A `Model` is what a scene says about drawing an entity: a mesh and a material, by asset.

```text
Model.mesh = asset("builtin#cube", 6f66656c6961206275696c74696e0001)
Model.material = asset("materials/red.material", 4f8e1d2c3b4a59687706f5e4d3c2b1a0)
Model.hidden = false
```

| Built-in | Asset |
|---|---|
| a cube of 1 × 1 × 1 | `builtin::CUBE` (`builtin#cube`) |
| a sphere of diameter 1 | `builtin::SPHERE` (`builtin#sphere`) |
| a plane of 1 × 1 facing up | `builtin::PLANE` (`builtin#plane`) |
| a cylinder of diameter 1 and height 1 | `builtin::CYLINDER` (`builtin#cylinder`) |
| a grey, half-rough material | `builtin::MATERIAL` (`builtin#default`) |

#### Meshes and materials from glTF files

A model can take its mesh and material from a glTF file (`.glb`, `.gltf`) in the game's
folder, by part:

```text
Model.mesh = asset("models/yard.glb#Yard 2", 5f2c0a3e9d2b47c88e51f0d4a7b3c912)
Model.material = asset("models/yard.glb#Yard 2 grid", 5f2c0a3e9d2b47c88e51f0d4a7b3c912)
```

- **A part** is a mesh or a material, by its name in the file or by `#index` (from 0:
  `models/yard.glb##1`). A file of one mesh may be named whole, without a part.
- **A mesh's primitives** are drawn as one mesh with the model's material, in the mesh's
  own space: the file's node transforms are not applied. A mesh of several materials is
  several models.
- **Textures** (base color, metallic-roughness, normal, occlusion, emissive), embedded or
  beside the file, are decoded the first time a material uses them and kept
  uncompressed; the shipped game's cooked textures are compressed. Shading models from
  extensions (unlit, clearcoat, sheen) are drawn as plain PBR here.
- **The file's id** is in `<file>.meta` beside it (`[asset version=1 id=…]`, ADR 0108),
  which the editor writes the first time it meets the file (`AssetMeta::ensure` in code).
  A `.meta` naming another id is reported as the asset's problem; a file without one is
  found by its path alone.
- **Files are read on first use** on the frame's thread, so a scene with large textures
  takes a moment on its first frame.

### Materials

A material is a file of its own, `.material`, with its id in the header:

```text
[material version=1 id=4f8e1d2c3b4a59687706f5e4d3c2b1a0]

base_color = [0.8, 0.15, 0.1, 1]
metallic = 0
roughness = 0.4
emissive = [0, 0, 0]
alpha = Opaque
alpha_cutoff = 0.5
double_sided = false
```

| Field | What it is |
|---|---|
| `base_color` | linear red, green, blue and alpha |
| `metallic` | 0 dielectric … 1 metal |
| `roughness` | 0 mirror … 1 matte |
| `emissive` | emitted light, linear (above 1 for bright emitters) |
| `alpha` | `Opaque`, `Mask` (cut below `alpha_cutoff`) or `Blend` |
| `double_sided` | both faces drawn |

- In code: `MaterialFile { base_color: [0.8, 0.15, 0.1, 1.0], ..MaterialFile::default() }`,
  written with `to_text(id)` and read with `MaterialFile::parse`.
- A scene finds the file by the path written beside its id, relative to the game's folder.
  A material file has no textures; a glTF's materials do (above).

## Changing components safely

- **Adding a field:** old scenes load with the field's default value.
- **Removing or renaming a field:** old lines are reported as warnings and ignored.
  Keep the old name with `#[reflect(rename = "old_name")]` to keep loading it.
- **Renaming a component:** put `#[reflect(name = "...")]` on the type to keep
  the name used in files.
- `#[reflect(skip)]` fields are never saved: caches, handles and runtime state.

## Merge conflicts

Every value sits on its own line. Different fields of the same entity merge without
conflicts. A real conflict (the same field changed twice) is one line. Keep one side;
there is no hidden structure to repair.

## Versions

- **Version 2** (ADR 0108) added strings and containers, names, asset references and the
  scene's id.
- **Version 3** (ADR 0108, format 3) added the roots' order: `order = <n>` in a root's
  block (`RootOrder` in the world), spaced so moving a root changes its own line only.
  Root blocks stay in id order. `order` on a child is a warning and is ignored.
- **Versions 1 and 2 load as they are** (their roots have no order). Saving writes
  version 3.
- **Prefab instances** (any scene placed in another, with its overrides) come with the
  editor's prefabs (ADR 0107, phase E5).
