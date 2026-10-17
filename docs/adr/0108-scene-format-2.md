# 0108. Scene format 2: names, strings and containers, assets by id and path, prefab instances

- Status: Accepted in part.
  - **Asset identity and references:** the owner's choice, 2026-10-17 ("давай смесь").
  - **Prefab instances:** proposed. The owner is to choose between any scene being a prefab
    (recommended) and a separate prefab file type.
  - Version 2 is frozen once projects use it. Later changes bump the version and ship a
    migration.
- Date: 2026-10-17
- Follows: [ADR 0029](0029-scene-format.md) (format version 1)
- Related:
  - [ADR 0104](0104-save-games.md): the saves' text form, whose value grammar this adopts;
  - [ADR 0101](0101-mesh-colliders.md): the colliders' rules file beside a scene, the first
    sidecar;
  - [ADR 0107](0107-editor.md): the editor;
  - research: `docs/research/editor-architecture.md` §4; UX: `docs/research/ux/prefabs.md`,
    `asset-browser.md`.

## Context

- **Version 1** writes plain data only: no strings, lists, optional values or maps
  (ADR 0029, "not yet supported").
- **Components refer to assets by runtime ids:** `MeshRenderer.mesh` is a `MeshId` in
  the geometry pool, meaningless on disk.
- **The editor needs** names (strings), asset references that survive renames, and prefab
  instances. The owner put prefabs in the first version (ADR 0107).
- **Saves already write** strings and containers in the same line grammar (ADR 0104 step
  2). One grammar should serve both.

## Options

### Where asset ids live (put to the owner, 2026-10-17)

1. **A local database** (SQLite or similar) of paths and ids.
   - If it stays on one machine, ids differ between machines and clones, and references in
     scenes kept in version control break.
   - If it is committed, it is a binary file that cannot be merged.
   - Good only as a cache, rebuilt from the files (Godot's `.godot/`, Unity's
     `Library/`).
2. **A small text file beside each asset** (Godot's `.uid` and `.import`, Unity's
   `.meta`).
   - The id travels with the asset in version control.
   - Import settings fit in the same file.
   - One extra file per asset. A file renamed outside the editor without it loses its id
     until it is found again.
3. **The id inside the file.** Works only for our own formats: a glTF, PNG or WAV cannot
   carry our id without changing someone else's file. Godot 4 began with ids inside
   scripts and moved to `.uid` files.

**Chosen: the mix** (the owner):
- our files carry their id inside (3);
- foreign files get a file beside them (2);
- references carry both the id and the path;
- the database is only a cache (1).

### Prefabs (to the owner)

1. **Any scene can be instanced** (Godot): a level, a room and a crate are one kind of
   file, and nesting falls out of it. **Recommended.**
2. **A separate prefab file type** (Unity).

Either way, overrides follow the stable ids of the prefab's entities, not their names or
paths: Godot and Figma lose overrides when a node or layer is renamed (`prefabs.md`).

## Decision

### Values: the saves' grammar

Version 2 writes and reads every value the saves' text form does (ADR 0104):
- strings: `"…"` with `\"`, `\\`, `\n`, `\t` and `\u{…}`;
- lists by index (`items.0.id = 3`), or `[a, b]` for lists of single values, `[]` when
  empty;
- maps by key in brackets (`flags["met the smith"] = true`);
- optional values: `none`, `some`, or straight to their fields;
- `{}` for an empty map or a value with no fields.

### Names

- **A `Name` component** (a string) gives an entity its name in the editor's tree. Names
  need not be unique.
- Entities are still identified by their 128-bit scene id (ADR 0029). Renaming changes
  nothing else.

### Ids in our own files

- **A scene's header carries its asset id:**

  ```text
  [scene version=2 id=9b41d07c2e8a4f6b8d3c1a5e7f902b64]
  ```

- **Other formats of ours** (materials when they become files, input maps, settings) carry
  theirs the same way.

### Files beside foreign assets

- **`<file>.meta` beside every source file** that is not ours (glTF, textures, sounds,
  videos, fonts, scripts):

  ```text
  [asset version=1 id=5f2c0a3e9d2b47c88e51f0d4a7b3c912]
  texture.compression = BC7
  texture.srgb = true
  ```

  It holds the id and the importer's settings (empty when the defaults apply), in the same
  line grammar.
- **The editor and the command line** make it the first time they meet a file without one.
  The editor moves, renames and deletes it with its file. The asset browser hides it.
- **A file moved outside the editor without its `.meta`** is found again by its old path
  in the cache's index, or by its content hash. Otherwise it gets a new id, and references
  to the old one show as missing assets with their paths (errors next to their cause).
- **The colliders' rules file** (`<scene>.colliders.ron`, ADR 0101) stays as it is for
  now. Folding it into the `.meta` is a later migration.

### References

- **A reference is written with its path first, then its id:**

  ```text
  MeshRenderer.mesh = asset("props/crate.gltf#Crate", 5f2c0a3e9d2b47c88e51f0d4a7b3c912)
  ```

  - The path keeps diffs readable.
  - `#…` names a part inside the file: a mesh or material of a glTF, by name or `#index`
    as the colliders' rules do.
  - The id is the truth.
- **On load:**
  - the id is looked up first (the cache's index gives its current path);
  - the path is the fallback;
  - where they disagree, the editor rewrites the path on the next save.
- **In Rust:** a reflected asset reference type (an id and the part's name) that the
  inspector, the writer and the reader know. It is resolved to runtime ids (`MeshId`,
  `MaterialId`, …) by the asset database when a scene loads. Runtime ids are never
  written. How `MeshRenderer` and the other components carry it is settled in E1 and E4
  (ADR 0107).

### The local cache

- `.ofelia/` in the project, never committed (ADR 0107 §4 and §5): the index of ids and
  paths, where each asset is used, import states, thumbnails, cooked data.
- It is rebuilt from the files at any time.

### Prefab instances (proposed: any scene is a prefab)

```text
[instance 7d2e0f1a9b3c4d5e8f6a7b1c2d3e4f50]
prefab = asset("props/crate.scene", 9b41d07c2e8a4f6b8d3c1a5e7f902b64)
parent = 6c1f0a3e9d2b47c88e51f0d4a7b3c912
Transform.translation = (3, 0, 1)

[override 7d2e0f1a9b3c4d5e8f6a7b1c2d3e4f50/a13f5c2e8b7d4e1f9a6b3c0d2e5f8a71]
Health.current = 50
remove = Light

[entity 0b12e4f8c3a94d5e9f7a6b1c2d3e4f50]
parent = 7d2e0f1a9b3c4d5e8f6a7b1c2d3e4f50/a13f5c2e8b7d4e1f9a6b3c0d2e5f8a71
Name = "Loot"
```

- **An `[instance]` block** places a scene. Its lines override the prefab's root.
- **An `[override <instance>/<entity>]` block** overrides an entity inside the instance,
  named by its id in the prefab's own file. `remove = <Component>` removes a component.
  `remove = @<entity>` in an instance block removes one of the prefab's entities.
- **Ids inside an instance:**
  - an entity of the prefab gets the id `<instance>/<entity>` in the containing scene;
  - in the world it gets a scene id derived from both (a hash), stable across loads, so
    references into an instance (`@7d2e…/a13f…`) and undo work.
- **Children added to an instance** are ordinary `[entity]` blocks whose parent is inside
  it.
- **Nesting:** a path of ids goes through instances inside instances
  (`7d2e…/0b12…/a13f…`).
- **A variant** is a scene whose root is an `[instance]` with overrides.
- **When the prefab changes,** instances follow. An override whose target no longer exists
  becomes a warning with its line and stays in the file until the user drops it, so a
  temporary removal loses nothing.
- **Order** stays deterministic (ADR 0029): instance and override blocks are placed in the
  tree's depth-first order, and their lines follow the component and field rules of
  version 1.

### Version and migration

- **`version=2`** in the header. Version 1 files still load. A scene loaded from version 1
  gets an id on its first save, which writes version 2.
- **Unknown fields** are warnings and **unknown components** errors, as in version 1.
  Overrides of fields the prefab's types no longer have are warnings.

## Consequences

- **Scenes, prefabs and saves share one value grammar.** One reader and one writer grow
  for both.
- **Renames inside the editor never break references.** Renames outside it are found
  again in most cases and reported where not.
- **One extra small file per foreign asset.** The asset browser hides them; version
  control keeps them beside their files.
- **The cache can be deleted at any time** and costs a re-import.
- **Merges stay line-local:** an override is one line, an instance one block.
- **Still to settle:** the prefab option (the owner), and how components carry asset
  references in Rust (E1, E4).
