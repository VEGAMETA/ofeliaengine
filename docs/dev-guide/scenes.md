# Scenes

Scenes are text files meant for version control. Two people can change the same
scene and merge like code.

```text
[scene version=2 id=9b41d07c2e8a4f6b8d3c1a5e7f902b64]

[entity 6c1f0a3e9d2b47c88e51f0d4a7b3c912]
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
- **Version 1 files load as they are.** Saving writes version 2.
- **Prefab instances** (any scene placed in another, with its overrides) come with the
  editor's prefabs (ADR 0107, phase E5).
