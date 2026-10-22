# 0108. Scene format 2: names, strings and containers, assets by id and path, prefab instances

- Status: Accepted (the owner's choices, 2026-10-17).
  - **Asset identity and references:** the mix ("давай смесь").
  - **Prefabs:** any scene can be instanced ("давай А по префабам").
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

### Prefabs (put to the owner, 2026-10-17)

1. **Any scene can be instanced** (Godot): a level, a room and a crate are one kind of
   file, and nesting falls out of it. **Chosen** (the owner).
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

### Prefab instances (any scene is a prefab)

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

## As built (E1-1, 2026-10-17)

Values, names, asset references and the header's id; prefab instances come with E5.

- **`engine_scene::text` holds the shared grammar:**
  - quoting, splitting lines and items, path steps;
  - following a path through structs, arrays, lists, maps and optional values;
  - reading a value into a reflected type.

  The saves' text form uses it in place of its own copy. Numbers are now parsed at their
  own width in both: an `f32` no longer goes through `f64`, which could round twice.
- **A struct of one unnamed field is written as that field:** `Name = "Door"`,
  `Health = 80`. Format 1's `Health.0 = 80` still loads.
- **`AssetRef`** is plain data, 24 bytes: the file's `AssetId` and the part's name as a
  64-bit FNV-1a hash (0 is the whole file). Components holding it stay copyable, so undo
  records them as bytes (ADR 0031).
  - Written as `asset("path#part", <file id>)`.
  - Without a known path it is written as `asset("#~<hash>", <file id>)`, which loads back
    to the same reference.
  - `AssetPaths` carries the paths: `LoadReport::assets` gives the ones a file was written
    with, and `SaveOptions::assets` takes them back (later, the editor's database).
- **`save_with` and `SaveOptions`** take the scene's id and the asset paths;
  `LoadReport` gains `id`, `version` and `assets`. `save` writes version 2 without an id.
- **`Name`** is registered by `SceneRegistry::with_engine_types`.

**Measured** on ref-1, `cargo bench -p engine_scene --bench scene`: 10,000 entities with a
`Transform` in 100 hierarchies of 100. A/B against main's build, 4 alternating runs:

| | main | format 2 |
|---|---:|---:|
| save | 7.28 ms | 6.49 ms |
| load | 9.72 ms | 9.65 ms |

- The first version of the reader loaded in 10.3 ms. It got back to main's time with:
  - plain `=` and `.` splits when a line has no quotes or keys;
  - a vector's numbers parsed in place;
  - the checks only structs need behind one kind test.
- With a `Name` on every entity: save 7.17 ms, load 11.86 ms. Most of a name's load cost is
  the second component moving each entity to another archetype (ADR 0029's known next
  step).

## As built (E1-2, 2026-10-17)

Scenes in the player.

- **`SceneRegistry::from_program()`:**
  - every reflected component linked into the program that has a `Default`, read and
    written through its component id (no Rust type behind the registry);
  - types marked `hidden` or `skip` stay out;
  - `MeshRenderer` is now `hidden`: its ids live only in memory.
- **Files of one value:** `write_value` and `read_value`. The header is
  `[<kind> version=<n> id=<id>]`, then the struct's lines without a component's name.
- **Material files** (`.material`, `engine_app::MaterialFile`, version 1):
  - fields: base color, metallic, roughness, emissive, alpha (`Opaque`, `Mask`, `Blend`)
    with its cutoff, double-sided;
  - textures come with the asset browser (E4).
- **`engine_app::Model`:** a mesh and a material by asset.
  - The player makes the entity's `MeshRenderer` in its frame, after the game's
    presentation and before extraction. The pools begin their frame after the
    simulation, so new meshes and materials reach the GPU the same frame.
  - It keeps the renderer in step:
    - a `Changed<Model>` query visits only chunks where models were written;
    - a marker takes the renderer away when the model goes.
  - An asset it cannot find leaves the entity undrawn and is said once in the log.
- **The built-in library** `builtin#<part>`, under one fixed id: cube, sphere, plane,
  cylinder (each 1 across) and the default material.
- **The player:** `PlayerSettings::scene` and `project`, `Player::load_scene`.
- **A model spawned with a `Transform` alone** gets the world transform propagation fills.
- **Tested:**
  - `engine_app` `scene_file`: a scene built in code is saved with a material file beside
    it, loaded through `PlayerSettings::scene` and rendered offscreen; removing a model
    removes its renderer;
  - `zero_alloc_models`: models shown and hidden in turn every tick, so the renderers
    follow every frame, without a heap allocation in the steady state.

## Format 3: the roots' order (team decision, 2026-10-20)

Format 2 wrote a scene's roots in the order of their ids, so the order the tree showed
them in was lost on the next load. The options put to the owner:

- sort the roots by name: nothing stored, but the order is not the user's;
- an `order = <n>` line in each root's block: the user's order, kept; moving a root
  changes its line only;
- the order of the blocks in the file: nothing more stored, but moving two roots moves
  whole blocks, with large diffs.

**Chosen: the `order` line** (version 3).

- A root's block holds `order = <n>` after its header; a child's holds `parent` instead.
  Root blocks stay in id order, so moving a root changes its `order` line only.
- In the world, the order is the `RootOrder` component (plain data, so undo keeps it).
  The editor's tree shows roots by it, those without one last in spawn order. A scene
  without orders (format 1 or 2, roots made by code) is numbered in the order shown when
  the editor first draws it.
- **Spaced values** (`RootOrder::STEP` = 1024): a new root goes after the last; a root
  placed between two others takes the middle of the gap. When no gap is left, every root
  is numbered again by steps, in the same undo step (ten copies of one root in a row do
  it once).
- `order` on a child's block is a warning and is ignored; a value that is not a whole
  number from 0 to 4,294,967,295 is an error on its line.
- **Versions:** saving writes version 3; versions 1 and 2 load as they are. An engine
  that knows only version 2 refuses a version 3 file ("newer than this engine") rather
  than misreading it.
- **Children** keep their parent's child order through the order of their blocks, as
  in format 2.

## Consequences

- **Scenes, prefabs and saves share one value grammar.** One reader and one writer grow
  for both.
- **Renames inside the editor never break references.** Renames outside it are found
  again in most cases and reported where not.
- **One extra small file per foreign asset.** The asset browser hides them; version
  control keeps them beside their files.
- **The cache can be deleted at any time** and costs a re-import.
- **Merges stay line-local:** an override is one line, an instance one block.
- **Still to settle:** how components carry asset references in Rust (E1, E4).

## As built (2026-10-21): models from glTF files

The player's `Assets` makes a `Model`'s mesh and material from a glTF file's parts (ADR
0110 E2b-6b, so a baked blockout draws from its file; the rest of E4 still to come):
- **Parts** by name or `#index` (from 0), as the colliders' rules name meshes; a file of one
  mesh named whole is that mesh. A mesh's primitives are merged into one mesh drawn with
  the model's material, in the mesh's space (node transforms not applied).
- **Materials:** the glTF material's factors, alpha mode and double-sidedness; its five
  textures decoded with their levels (`build_mips` for their use, as the cooker filters
  them), uncompressed (RGBA8), made where the device is at hand after the frame's
  stages, in the same frame. Extension shading models are plain PBR in this path.
- **`.meta`:** `AssetMeta` (`[asset version=1 id=…]`, no importer settings yet); the
  player checks a file's `.meta` id against the reference's and accepts a file without
  one by its path; `AssetMeta::ensure` gives a file one (the editor's first meeting).
- **Read on first use** on the frame's thread, each file once per frame, dropped once its
  parts are made. Moving the reads to a loading thread and compressing textures through
  the derived data cache are E4's.

## As built (E4-1, 2026-10-22): the asset database

`engine_scene::AssetDatabase` (ADR 0107 §5's index, its form chosen by measurement):
- **What it holds:** every asset file under the game's folder by kind (scene, material,
  model, texture, sound, video, font, script), its id, path, size, modification time,
  and the content's hash for files whose `.meta` it wrote.
  - Ids come from our headers and from the `.meta` files (`AssetMeta`, moved here from
    `engine_app`).
  - A foreign file without a `.meta` gets one: the id of the file gone from the same path,
    or of a file gone with the same size and content, else a new id.
  - Two files with one id: the later path gets a new `.meta`; our own files are reported.
- **The index:** `.ofelia/assets.index`, our own text format (a header, then a line per
  file, tab-separated), gone files kept for their ids.
- **Measured** (ref-1, release, `measure_ten_thousand_assets`, 10,000 files in 100
  folders):

  | Step | Time |
  |---|---|
  | First scan, every file read, its `.meta` written | 1,306 ms |
  | Index written (986 KB) | 2.0 ms |
  | Index read | 3.2 ms |
  | Unchanged project scanned | 20 ms |

  The unchanged scan takes the `.meta` files' presence from the folder listing, not from
  a stat per file (167 ms before).
- **Decided on that:** the cache is our own text file, not SQLite (a C dependency for
  nothing at these sizes); at that rate a project of
  100,000 files scans in about 0.2 s (not measured).
- **In the editor:** opened, scanned and saved when the editor starts, every file's path
  given to the player by id; the folder watched (`DirectoryWatcher`), a change scanned
  after 0.15 s of quiet. `ofeliaengine assets [DIR] [--json]` does the same from the
  command line.
- **Left:** a file moved without its `.meta` is found by content only if the database
  wrote that `.meta` (others' hashes come with background hashing); renames inside the
  editor and the browser (E4-2).

## As built (E4-4, 2026-10-22): glTF files load in the background

**Context.** The player made a glTF model's parts on first use on the frame's thread:
- the file read and imported;
- its meshes put together and their vertices encoded;
- its textures read, decoded and given their mip levels.

FlightHelmet (6 meshes, 15 textures of 2048²) held one frame for 1,110 ms. In the editor
that froze the window when a model was dropped or a scene opened. CLAUDE.md §5: asset
import never blocks the UI.

**Where the time went** (FlightHelmet on one thread, release, ref-1):

| Step | Time |
|---|---|
| import | 4 ms |
| the textures read | 9 ms |
| decoding | 293 ms |
| mip levels | 1,023 ms |

The mip levels were mostly the linear → sRGB encoding (a walk through a table with a serial chain of loads) and the resampling's vertical pass, a texel at a time. Both are faster now, to the bit (ADR 0040's addendum of the same day).

**Options:**
- **a) Keep the frame's thread, one texture a frame.** Still a frame of 40–70 ms for each
  2048² texture, and an import cannot be split.
- **b) A loading thread with a pool of workers below normal priority, as the streaming
  thread (ADR 0057).** The frame's thread gives jobs and takes each result back as soon
  as it is done, then makes the GPU's mesh or texture (the pools spread the copies over
  frames).
- **c) The frame's job system.** Its workers would hold long jobs through frames and
  compete with the frame's own work.

**Decision: b** (`engine_app::loading`):
- **Jobs and results:**
  - jobs go through a queue to the thread, which runs the batch it finds on its pool (half
    the cores, at most 8, below normal);
  - each result comes back on a channel as soon as its worker is done;
  - the thread starts with the first job and parks when idle.
- **The jobs:**
  - a file read and imported;
  - a mesh put together, its vertices encoded as the pool keeps them
    (`GeometryPool::add_mesh_with_attributes`, added for this);
  - a texture read, decoded and given its mip levels.

  A panic in a job is that asset's failure, not the thread's.
- **The frame's side:**
  - a model draws once its mesh and material are made, and the material's textures fill
    in as they come;
  - models waiting are tried again when something comes back;
  - a file read again (`reload`) raises its generation, and older results are dropped;
  - a file is let go a frame after its parts no longer need it.
- **Waiting:** the frame after a scene is loaded (`load_scene`, and the first frame) waits
  for its assets, so a scene shows whole as before, and so does one asked for with
  `Player::wait_for_assets`. The editor asks for it after a bake, so the model shows as
  the brushes go. A wait gives up after 10 s.

**Measured** (ref-1, release, `engine_app` tests/asset_loading.rs, `--ignored`):

| | FlightHelmet | Sponza (one mesh) |
|---|---|---|
| Before, the frame that first shows it | 1,110 ms | 103 ms |
| Added later: the worst frame while it loads | 6.5–8.7 ms | 4.0–4.9 ms |
| Added later: shown after | 225–290 ms | 38–43 ms |
| In the first frame, which waits: that frame | 208–285 ms | 45–48 ms |

- Before the vertices were encoded on the thread, Sponza's one mesh of 192,496 vertices
  cost the frame that made it 15 ms; now it costs 4–5 ms.
- What is left of the worst frame is the frame's own share of making the meshes and
  textures that came back.

**Left:**
- a model read again shows nothing until its new version is made (the old one could stay
  until then);
- the Sponza here is one mesh of 25 materials, which draws with its first (the format
  question of E4-3), so 3 of its textures are made;
- a loading screen's progress (`Assets::loading` counts what is in flight).

## As built (E4-5, 2026-10-22): files written anew are made again

CLAUDE.md §13: hot reload for any asset, a file watcher to a reimport to a swap at run
time.
- **Seen:** the asset database's scan names the files written anew (`ScanReport::changed`:
  at the same path with the same id, of another size or time; not new or moved ones). The
  editor's watcher already scans the folder after 0.15 s of quiet (E4-1).
- **Made again:** `Player::asset_changed(id, path)` (`Assets::changed`) reads the file
  again in the background (E4-4). An image beside glTF files makes the files that read it
  again: a file notes its images as it is read.
- **No gap:** what was made from the file becomes stale. It is drawn until its new version
  is whole: a mesh once made, a material once its textures are.
  - A model drawing a stale part keeps waiting, and is tried again when results come or a
    material settles.
  - A stale part goes once no model draws it: at its replacement, or once nothing waits.
  - A file read again while already being read lets the older stale parts go.
  - A broken new version keeps the old one drawn, its problem in the log.
- **Checked:**
  - a model drawn every frame while it is made again, then from the new file, the old
    texture let go;
  - an image painted again makes the glTF that uses it again, and one nobody uses makes
    nothing;
  - in the editor, a model's file written anew outside it is made again, drawn all along.
- **Left:**
  - the editor's own bakes are seen by the watcher too and made again once more, which
    costs a reload of a file just made;
  - scenes and scripts written anew are not reloaded here (scripts come with Luau,
    scenes with the session's conflict rules, ADR 0107 §4).

## As built (E4-8, 2026-10-22): textures through the derived data cache

The textures of glTF files the player makes go through ADR 0041's cache, as Unreal's
editor compiles textures in the background and Unity's caches its imports:
- **Where:** `PlayerSettings::derived_data` (`Assets::set_derived_data`), a folder.
  - The editor gives its project's `.ofelia/ddc` (ignored by version control, as all of
    `.ofelia/`).
  - `None` (the default, a game run without it): textures stay uncompressed, as before.
- **A texture in the cache** (`engine_assets::cached_texture`: the key of its image file's
  bytes and its use, the cooker's own) is loaded from it compressed: BC7 for color and
  data, BC5 for normal maps. Nothing is decoded and no mip level is computed.
- **A texture not in it** is made as before, uncompressed, so the model shows as fast as
  without the cache. Then it is compressed on a second thread of the loader:
  - its pool runs at the lowest priority but one (`Lowest`), below the loading's workers
    (`BelowNormal`) and the frame (`Normal`): compression never holds a load or a frame
    back;
  - two textures at a time, each over every worker (one decodes and filters its levels
    while the other's blocks keep the workers busy), the thread helping;
  - the job keeps the texture's image only (its path, or the bytes of an embedded one),
    not its glTF file;
  - the result goes into the cache and back to the frame.
- **The swap:** the compressed texture is made beside the uncompressed one and swapped in
  once every level is uploaded and shown (`TexturePool::shown_level` is 0), so the picture
  neither blurs nor loses the texture for a frame. The materials using the old one are
  pointed at the new one; the old one is freed from the next frame on.
- **Not loading:** compression is not counted in `Assets::loading` (the frame after a
  scene is loaded does not wait for it); `Assets::compressing` counts it, and the editor's
  status bar shows "Compressing textures: N".
- **Files written anew** (E4-5): what was compressed from the old version is not swapped
  in; the new image is made and compressed as a new entry.
- **Measured** (ref-1, release; FlightHelmet: 6 meshes, 15 textures of 2048²):

  | FlightHelmet | Without a cache | Empty cache | Filled cache |
  |---|---|---|---|
  | Shown, in the first frame (it waits) | 273 ms | 279 ms | **34 ms** |
  | Shown, added later (a drop) | 321 ms | 363 ms | **31 ms** |
  | Worst frame while loading, added later | 6.2 ms | 8.0 ms | 9.0 ms |
  | Textures' GPU memory | 272 MB | 272 → **68 MB** after 8.1 s | **68 MB** |

  - The worst frame while compressing in the background is 5.2 ms (a drop): the frame's
    own work for a swap is under 1 ms.
  - From the cache a dropped model's meshes and textures arrive within a few frames, so
    their uploads come together (9.0 ms at worst, once); making the 15 textures in one
    frame costs under 1 ms.
  - On one thread, the 15 textures cost 297 ms to decode and 600 ms for their levels
    uncompressed; from the cache, 42 ms of keys (FNV-1a over 45 MB of files at 1.1 GB/s)
    and 19 ms of entries read.
- **Left:**
  - the keys' hash was 70 % of a load from the cache: XXH3-128 since ADR 0113 (keys
    42 → 1 ms on one thread; FlightHelmet from the cache 31 → 27 ms added later, 34 →
    31 ms in the first frame);
  - compressing on the GPU (ADR 0041's next) would make the 8 s of the first time
    vanish.

## As built (E4-9, 2026-10-22): import settings

The asset browser research's "import settings in the inspector" (Unity, Godot), kept in
the `.meta` as this ADR planned:

```text
[asset version=1 id=5f2c0a3e9d2b47c88e51f0d4a7b3c912]

texture.compression = Uncompressed
model.scale = 0.01
```

- **`AssetMeta`** holds the settings (`engine_scene`: `TextureImport`, `ModelImport`).
  - Only the settings that are not the defaults are written, so a `.meta` of defaults is
    its header alone, as before.
  - A copy given a new id (two files with one id) keeps the settings it was copied with.
- **`texture.compression`** (`Auto`, `Uncompressed`): whether the player block-compresses
  an image through the derived data cache (E4-8). The setting comes from the image file's
  own `.meta`, or from its model's for an image inside the file. `Uncompressed` keeps
  RGBA8: pixel art, gradients and text that BC7 would smudge.
- **`model.scale`** (from 0.0001 to 10,000; 1 by default) multiplies a model's mesh
  positions in the player. The editor's drop (and its previews) also multiplies the
  file's node offsets and the model's box, so a file in centimeters drops whole at 0.01.
  A value that is not a positive number applies as 1.
- **The inspector:**
  - a file picked in the FileSystem dock (or from the palette's `#`) is shown in the
    inspector instead of the entities, as Godot's inspector shows a resource, until the
    entity selection changes;
  - "Asset" shows its path and kind; "Import" shows its settings (a model: scale and its
    textures' compression; an image: compression);
  - an edit writes the `.meta` at once and the asset is made again in the background,
    with nothing to press. A scale drag (doubled every 100 pixels) is written at its
    release.
  - An edit is not an undo step: a `.meta` is a file on disk, as a rename is, and an undo
    step would also mark the scene unsaved. ↺ (or Backspace over the field) puts a
    setting back to its default.
- **Left:**
  - a `.meta` edited outside the editor is not seen by the watcher (the database follows
    the asset files' sizes and times, not their `.meta`'s): the asset is made again on
    its next reading;
  - the cook (`ofeliaengine cook`) does not read the settings yet; the game's build from
    the editor will.

## As built (E5-1, 2026-10-22): prefab instances in the format

The section "Prefab instances" above, built in `engine_scene` (`prefab` module), format 4:

- **Ids:** `SceneId::derive(instance, local)` hashes both (StableHasher). A path names an
  entity through instances: `SceneId::of_path([a, b, c]) = derive(a, of_path([b, c]))`,
  a fold from the right, so placing a prefab under instance `I` maps each of its ids
  `x` to `derive(I, x)` and a path written in a scene names the same entity that nesting
  places.
- **The root (decided here; the format did not say):**
  - a prefab of one root: that root is the instance's entity and takes the instance's
    id, so `[instance]`'s lines override its values, as the format reads;
  - a prefab of several roots ("any scene is a prefab": a level of many roots): an
    entity of the instance's own holds them, with the instance's id and the instance's
    lines.

  A variant (a scene whose only root is an instance) falls out of the first rule: the
  outermost instance's prefab is the one its root shows.
- **In the world:** `PrefabInstance { prefab }` on an instance's root, `PrefabPart {
  instance, local }` on what it placed (plain data, so undo keeps them), `KeptBlocks` for
  what could not be applied.
- **Loading** (`load_with` with `LoadOptions { prefabs }`; `Prefabs` gives a prefab's text
  by id and path, `PrefabFiles` reads them from a folder, checking each file's id):
  - every prefab reachable is read once and parsed; one that would place itself, cannot
    be read, or that loading alone would refuse is not placed;
  - the plan places each instance's prefab with its ids mapped, then applies the scene's
    lines over it: a component's lines after the prefab's, `Component = {}` among them
    starting it from its default, `remove = Component` and `remove = @<entity>` (with its
    children);
  - an instance not placed is its entity alone (its lines place it), and its blocks are
    kept; an override of an entity the prefab no longer has is a warning, its block kept;
    both are written back as they were;
  - `load` (no prefabs) keeps every instance that way, so a scene round-trips unchanged
    through code that knows nothing of prefabs (the editor until E5-2).
- **Saving** (`SaveOptions { prefabs }`): each outermost instance's prefab is loaded alone
  under the same ids (a scene of that one instance block); an instance's root and each
  entity it placed are compared line by line with theirs. What differs is written; a
  component whose lines name other fields (a list grown or shrunk) is written whole after
  `Component = {}`; components and entities gone are `remove` lines. References to
  entities inside instances are written as paths.
- **The player** loads scenes with the project's folder as their prefabs' source.
- **Measured** (`engine_scene` bench `scene`, ref-1, 10,000 entities with no instances;
  main before → this):
  - first build: save 6.5 → 10.2 ms, load 9.8 → 14.3 ms;
  - after the plain path was made as lean as before (lines straight to the text, the
    writer's path kept, the plan borrowing its blocks' lines, rare fields boxed, no second
    id map): save 6.47 → 6.56 ms, load 9.68 → 9.90 ms (with names 7.01 → 7.10, 12.06 →
    12.23).
- **Left for E5-2 to E5-4:** the editor loading and saving through prefabs, making and
  unpacking instances, overrides in the inspector, prefabs edited live. Undo does not
  keep `KeptBlocks` (not plain data, not a scene type): deleting an instance that has
  them and undoing drops them.
