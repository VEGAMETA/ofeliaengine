# ADR 0126: The one-button game build (E14)

- Status: **Accepted** (2026-10-31, team decisions 1а 2а 3а 4а 5а: every question as
  recommended; on 5, `builds/` by default and another folder may be chosen, and a new
  game's `.gitignore` names `/builds/`). The questions at the end keep their options for the
  record.
- Research: [the one-button game build](../research/game-build.md) (Godot's presets and
  PCKs, Unity's Build Profiles, Unreal's cook/stage/package and IoStore, Fyrox's exporter,
  Bevy).
- Builds on [ADR 0056](0056-cooked-packs-and-streaming-reads.md) (the pack), [ADR 0057](0057-texture-streaming.md) and
  [ADR 0076](0076-gpu-memory-budget-and-streaming.md) (streaming), [ADR 0092](0092-custom-materials.md)
  (PSO records, entry points a game draws), [ADR 0102](0102-localization.md) (the strings
  pack), [ADR 0105](0105-sound-in-the-player.md) (the sound pack), [ADR 0107](0107-editor.md)
  §7 (E14) and [ADR 0108](0108-scene-format-2.md) (assets by id, `.meta`),
  [ADR 0111](0111-shared-engine-build.md) (games share one engine build),
  [ADR 0114](0114-luau-api.md) (scripts), [ADR 0118](0118-shipping-panic-strategy.md) (the
  shipping profile).
- Team order (2026-10-31, а): the parts of E14 a game needs to ship come first, this one
  first of them.

## Context

- **What a game is today:** a Rust crate made by `ofeliaengine new`, its scenes
  (`.scene`), materials, graphs, clips, scripts and source assets (glTF, images, sounds)
  in its folder. The player reads that folder while it is developed: it parses glTF in the
  background, encodes textures to BC7 through the derived data cache (`.ofelia/`), and
  compiles material types and graphs with Slang on its compiler thread.
- **What exists for shipping:**
  - the `shipping` profile (fat LTO, one codegen unit, unwinding; ADR 0118); the editor is
    behind a feature a shipped game does not turn on (ADR 0107: it costs nothing there);
  - `ofeliaengine cook`: a glTF scene into a pack (meshes with meshlets, textures,
    materials and the entry points a game draws, colliders baked for Jolt, a navigation
    mesh); `loc build` (strings pack), `sounds build` (sound pack), `shaders build`;
  - packs streamed within the GPU memory budget (`engine_streaming`, IoRing);
  - the engine's own SPIR-V embedded in its binary.
- **What is missing:** one step that makes a folder a player runs: the game built in its
  shipping profile, the content its scenes use cooked, no Slang and no glTF parsing at run
  time (`engine_shader`: "games ship precompiled SPIR-V and never load it"), the third
  parties' notices beside it. E2b's blockout cook and E4-9's import settings wait for it.

## Options (for the main question, the content's form)

1. **Cooked packs** (Unreal's cooked containers, Godot's imported resources in a PCK): the
   cook extended from one glTF scene to the game's scenes and everything they reach;
   meshes, BC7 textures, colliders, navigation, materials with their SPIR-V, graphs,
   clips, scripts as bytecode, in packs the player streams. Fast loading (zero-copy,
   IoRing), no sources shipped, nothing compiled on the player's machine but its PSOs.
   Cost: the largest; the player needs a path that reads scenes' assets from packs.
2. **Source files and the derived data beside them** (Fyrox's copy, Bevy's folder): the
   used sources copied, the cache's BC7 textures and SPIR-V shipped with them. Quick to
   make; slower first loads (glTF parsed at run time), larger, the sources readable, and
   material types still need their SPIR-V made at build time anyway.
3. **2 first, 1 after:** a runnable build soon, the cooked one replacing it.

## Proposal (the recommendations of the questions below)

- **A build file** in the project's grammar (`game.build`, committed): the
  start scene and the other scenes, folders always included (what scripts load by path),
  the profile, the output folder, the game's name and icon. The editor's Build settings
  edit it; its first save makes it from the open scene.
- **What goes in:** the scenes listed and every asset they reach by id (prefabs, models,
  materials and their types or graphs, textures, clips, graphs, sounds, scripts and what
  they `require`), plus the folders always in; editor files (`.ofelia/`, autosaves,
  thumbnails) never.
- **Steps, each cached:**
  1. the game built (`cargo build --profile shipping`, or `release` for a quick test
     build), the editor feature off;
  2. the content cooked into packs (option 1), only what changed since the last build
     (the derived data cache's keys, [ADR 0113](0113-content-hash.md));
  3. the strings and sound packs, the PSO records played in the editor;
  4. the folder written: `<Game>.exe`, `data/`, `plugins/` (empty: where a player puts the
     vendors' DLLs, CLAUDE.md §6), `THIRD_PARTY_NOTICES.txt` (MIT, Apache and BSD
     dependencies require it).
- **One code path:** the editor's **Build** (a task in the Tasks panel, never modal; errors
  in the Output linked to their asset or line) and `ofeliaengine build [DIR] [--json]` run
  the same library; `--json` serves the Claude Code layer (5a).
- **Checked by a test:** the quick start's game built, then run from its folder offscreen,
  its first frame compared with the editor's (FLIP, as the golden tests).

## Phases

| Step | What |
|---|---|
| **G1** | the build file, the dependency walk, `ofeliaengine build`: the game built, its scenes and the assets they reach in the folder as they are now read (a runnable build, option 2's shape), notices, `plugins/`; the run-from-the-folder test |
| **G2** | material types and graphs to SPIR-V at build time, the player loading them (no Slang at run time) |
| **G3** | the cook from the project: models, textures, colliders, navigation into packs; the player reading a scene's assets from packs. In three: **G3a** the derived data (compressed textures, cooked colliders) in one read-only pack beside the sources; **G3b** the images the pack stands in for left out (the models stay glTF, below); **G3c** navigation baked (moved to E14's navigation settings: below) |
| **G4** | the editor's Build: the settings section, the task, errors at their causes, Run the Build |
| **G5** | scripts as Luau bytecode, the PSO warm-up list, a size report, an optional zip |

## As built (G1, 2026-10-31): `ofeliaengine build` and a folder that runs

- **`engine_build`** (a new crate, the one code path the editor's Build will call):
  - `BuildFile` (`game.build`, the project's grammar, kind `build` version 1): `name`,
    `start`, `scenes`, `always`, `kind` (`Build` = cargo's `release`, `Ship` =
    `shipping`), `output` (`builds` by default, or another folder, absolute or under the
    game's). `BuildFile::for_game` keeps `scripts/` and `loc/` always in when the game has
    them, and the `.mixer` files at its root.
  - `plan`: the start scene and the scenes, then every file reached: an `asset("path", …)`
    in any text file (scenes, prefabs, materials, graphs, clips, mixers), a glTF file's
    `uri`s (percent-decoded; `data:` none), each file's `.meta`; the folders always in,
    whole; dot-files and dot-folders never. Files named and missing are listed with what
    named them, and stop the build before cargo runs.
  - `build`: cargo builds the game (`--profile release|shipping`, the `editor` feature
    off, a `CARGO_TARGET_DIR` around it removed so the game's own `.cargo/config.toml`
    decides, ADR 0111), its program found in cargo's JSON messages; then `write_folder`:
    `<name>.exe`, `data/` (a file of the same size and no older kept, what the last build
    had beyond the plan removed with its empty folders), `plugins/`, and
    `THIRD_PARTY_NOTICES.txt` (the engine's, embedded at its build); the game's `sounds/`
    folder becomes `data/sounds.pack` (made again only when a sound is newer), since a
    released player reads the pack (ADR 0105).
- **`ofeliaengine build [DIR] [--ship | --build] [--out DIR] [--json]`**: makes
  `game.build` the first time (the package's name, `scenes/main.scene` or the first scene
  found); `--json` prints one line (`ok`, `folder`, `program`, `kind`, `files`, `bytes`,
  `copied`, `removed`, `compile_seconds`, `seconds`; or `ok: false`, `error`, `missing`).
- **`ofeliaengine new`** writes `game.build` and adds `/builds/` to the game's `.gitignore`
  (the team's 5а).
- **The program** (`engine_app::run`): with no project in its settings and a `data/`
  folder beside it, that folder is its project and its working folder.
- **Measured on ref-1** (a game from `ofeliaengine new`, the engine already built in
  `dev` for another game): the first Build 101 s (the engine in `release`), the next after
  a change in the engine 15.7 s; the folder 28 MB (the program; data 2 KB and the sound
  pack); run from another working folder with its window hidden, it loaded its scene with
  no warning and ran until stopped.
- **Tests:** the plan (scenes, materials, a glTF's buffer and a percent-encoded image, a
  `.meta`, scripts always in but their dot-files, the sound pack planned, a missing scene
  and a missing buffer with what named them); the build file read back as written; the
  folder written, then again unchanged (nothing copied, a stale file and its folder
  removed, the made pack kept); the paths, names and cargo's JSON; the command line's
  options and `--json`'s escaping; `ofeliaengine new`'s `game.build` and `.gitignore`.
- **Left for G2–G5:** ~~material types and graphs still compile with Slang on the player's
  machine (G2)~~ (done, below); models, textures and the rest are copied as they are, not cooked (G3); the
  strings pack (`loc/` is copied whole for now); the editor's Build (G4); Luau bytecode,
  the PSO warm-up list, a size report, a zip (G5); the licenses of a game's own crates.io
  dependencies are not gathered yet.

## As built (G2, 2026-10-31): material types compiled ahead

- **The material pack** (`engine_assets::material_pack`, `data/materials.pack`): for each
  material type the build's material files name, its module's path under the game's
  folder, its name, its parameters' layout (Slang's reflection of `<type>.Params`) and its
  fragment entry points' SPIR-V; zstd level 19 behind a header with the build's key, read
  without the body.
- **The build** (`engine_build::materials`): before cargo, each typed `.material` in the
  plan read with its bases as the player reads it; a hand-written module's type, or a
  graph's module (generated into `.ofelia/cache/graphs/` and added to the plan, so the
  player finds it unchanged and writes nothing); each type once; a shader outside the
  game's folder or a material that does not resolve stops the build, naming the file.
  After the folder: the key (XXH3-128 of the engine's shader folder, the plan's Slang
  modules and the types) compared with the pack's; another key compiles every type
  (`SlangMaterialTypes`: layout, then all fragment entry points on up to 8 threads). The
  engine's shader folder comes from `Options::engine_shaders` (the CLI passes its own).
  `--json` gains `material_types`.
- **The player** (`engine_app::Assets`): a `materials.pack` in the project's folder is
  read with the assets; a type found there is registered with its SPIR-V and its layout
  (`ForwardRenderer::register_material_type_packed`): its parameters are laid out at once,
  its pipelines made by the warm-up thread, no compiler thread started. A type not in the
  pack, or an entry point it lacks, compiles as before, where Slang is at hand.
- **Measured on ref-1:** the stripes type and a graph's type, 318 fragment entry points,
  a 696 KB pack in 11.4 s; a type drawn from a pack 4 frames (0.07 s) after the scene
  loads, against 1.12 s compiling it. A game from `ofeliaengine new` with its cube's
  material typed: built in 31.1 s (24.9 s cargo, the type in about 6 s), again in 0.4 s
  (the pack kept); run with no Slang on the machine (neither `VULKAN_SDK` nor the search
  path), it draws the type with nothing logged, while without the pack it logs "no Slang
  compiler" and draws the material's look.
- **Tests:** the pack read back as written (header key alone, damaged and foreign files
  refused); the plan's materials naming their types once (a variant through its base, a
  standard material none, a graph's module made and planned, the key following a module's
  text, a shader outside the folder refused); the types compiled into a pack and a pack of
  the same key kept; the player drawing a packed type whose module was spoiled.
- **Left:** every fragment entry point of `forward.slang` is compiled for every type (159
  each): the PSO record of G5 keeps the ones playtests draw. The key does not hold Slang's
  own version (a Slang update with no shader change keeps the old pack; delete it).

## As built (G3a, 2026-10-31): the derived data in `derived.pack`

Without the editor's derived data cache a built game's player kept every texture as
decoded RGBA8 (four times the memory of BC7, no compressed mips) and cooked a model's
colliders again on every run. G3a carries what the editor's cache would hold.

- **`engine_assets::DerivedDataCache`** gains a second store: `open_pack` reads
  `derived.pack` (`"OFDD"`, version, an index of key, kind, offset and length, then the
  entries) as a cache that has its entries and stores nothing (`put` fails, `read_only`);
  entries are read with positional reads of one open file, from any thread. A cache made
  `recording` notes every entry read or stored through it (`used`), and `write_pack` packs
  those.
- **The build** (`engine_build::derived`, after the folder is written): through the
  editor's own cache (`.ofelia/ddc`, so what the editor made is not made again, and what
  the build makes the editor finds), recording, the same imports the player makes and with
  the same keys: every glTF's textures for their uses (`engine_assets::cook::texture_uses`,
  now public, the uses the player keys its textures by) and its colliders by its rules
  (`cook_node_colliders`, as `Assets::collider` cooks them); typed materials' texture
  parameters for their encodings; the other images planned (sprites) as color. An image
  whose `.meta` keeps it uncompressed is left out, as the player leaves it. A pack with the
  same entries is kept (the keys hash the sources and the settings).
- **The player:** with no derived data cache in its settings and a `derived.pack` in its
  project, that pack is its cache; with a read-only cache nothing is compressed while the
  game runs (a texture missing from it stays as decoded).
- **Measured on ref-1:** a game with FlightHelmet (15 textures of 2048²): 7.6 s to
  compress them the first time, 0.1 s again (the pack kept), a 71.3 MB pack; the player
  then loads them as the editor's warm cache does (E4-8: 8 times faster than decoding,
  a quarter of the memory).
- **Tests:** a sprite, an image kept uncompressed and a typed material's normal map: the
  two compressed for their uses, found by the player's lookup, the pack read-only and
  kept when nothing changed; a built game's player drawing two models (an image file, an
  image inside the glTF) with both textures compressed from the pack, nothing compressed
  and nothing written beside it.
- **Left for G3b:** the sources still go in beside the pack (the keys are their bytes'
  hashes); the glTF files are still imported when loaded; HDR panoramas (the sky) are not
  in the pack.

## As built (G3b, 2026-11-01): the images left out

- **Decision (team, 2026-11-01):** G3b leaves out the images, not the models. A built
  game's weight is its images (FlightHelmet: 45 MB of PNG beside a 3 MB `.gltf` and
  `.bin`), and they were already in the pack compressed; a glTF's `.bin` is the meshes'
  binary data, read as it is. A cooked model format (no JSON, meshes laid out for the GPU,
  meshlets) waits until a measurement shows glTF loading costs a built game's start; the
  scene packs of ADR 0056 remain the cooked form for streamed worlds.
- **The pack names its images' contents:** an entry of kind `source`, keyed by the
  image's path under the game's folder (`engine_assets::source_key`), holds the XXH3-128
  of its bytes; `texture_key_of(content, usage)` is the texture key without the bytes;
  `DerivedDataCache::packed_texture(file, usage)` finds a texture by its file's path under
  the pack's folder.
- **The build** records each compressed image's content and leaves out the images the
  models and material files use (`Derived::left_out`), before the folder is written.
  Sprites' images (and any other image a game may read by path) and images kept
  uncompressed go in.
- **The player's loader** asks the pack before reading an image (a material type's
  parameter, a glTF's image file); an image inside a `.glb` is read from it as before.
- **Measured on ref-1:** the FlightHelmet game's `data/` from about 120 MB to 72 MB (its
  15 images left out; the pack is 71.3 MB of it); the build as fast as before (7.1 s the
  first time, 0.1 s again).
- **Tests:** the material's image left out and found by its path under the pack's folder
  (for its use only), the sprite's kept; a built game's player drawing a model whose image
  file was removed, its texture compressed from the pack.

## G3c moved (2026-11-01)

A scene file's game has no navigation mesh to bake yet: the player reads one only from a
cooked scene pack (`Player::load_navigation`, ADR 0103), scripts have no paths API, and the
editor bakes from the blockout only to draw it (View: Navigation). Baking at build time
would have no reader. It moves to E14's navigation settings, which add the agent's settings
to the project, the bake the editor's Play and a built game share (at build time into the
derived data, at load in the editor), and the paths API.

## As built (G4, 2026-11-01): the editor's Build

- **Commands** (the palette's Build group): **Build the Game** (Ctrl+Shift+B), **Ship the
  Game** (the shipping profile), **Run the Last Build**, **Open the Build File**
  (`game.build` in a code tab, made from the open scene the first time, as a first Build
  makes it). They work from every tab.
- **In the background** (`engine_editor::game_build`): `engine_build::build` on a thread
  of its own, one build at a time; the Tasks panel's row "Building the game's folder" says
  its step (compiling, compressing textures and cooking colliders, compiling material types,
  writing n/m) and how long it has run.
- **Errors at their causes:** cargo's lines go to the Output through the log (target
  `build`): errors and warnings at their levels, their `--> file:line:col` rows links to the
  place; the crates compiled as detail. A file named and missing is a row naming the file
  that names it (a link to it). The outcome is said in the status line and the Output.
- **One code path:** the build file is made by `BuildFile::open_or_make`, now the
  library's, which `ofeliaengine build` calls too.
- **Not done:** an inspector section for the build file's fields (the file is edited in its
  code tab for now); the build reads the files as saved, so a scene's unsaved changes are
  not in it; the build's process is not lowered in priority.
- **Tests:** cargo's lines at their levels; a folder that is not a game's said at once with
  no file made; a missing file's report.

## As built (G5a, 2026-11-01): the PSO record from Play

- **The editor records** at each Stop the material types' entry points its frames drew
  (`ForwardRenderer::material_type_entries_drawn`) into `game.pso.ron` beside `game.build`
  (`engine_build::PSO_RECORD`; ADR 0092's record, merged, only growing, sorted, so the
  team commits and shares it); nothing is written when no type was drawn or the record
  already has them.
- **The build** compiles a type the record names for the entry points it lists only
  (`SlangMaterialTypes::with_pso_record`); a type it does not name whole, as before; the
  record is part of the material pack's key.
- **Measured on ref-1:** the stripes type recorded with one entry point and a graph's type
  not recorded: the pack from 696 KB (318 entry points) to 154 KB, the compile from 11.4 s to
  about 5 s.
- **Left:** warming the recorded pipelines up at load (they are made when first drawn, the
  look drawing meanwhile); a test of the record written at Stop (the build's side is
  tested).

## Questions for the team

1. **What form the shipped content takes.**
   - **а) Cooked packs** (recommended; option 3's order: G1 makes a runnable build of copied
     files first, G2–G3 replace them): fastest loading, nothing compiled on the player's
     machine but its pipelines, no sources shipped. Cost: G2–G3, about 8 PRs.
   - б) Copied sources and the cache's data only (option 2): about 3 PRs, slower first
     loads, larger, the sources readable.
2. **What goes in.**
   - **а) The scenes listed and what they reach, plus folders marked "always in"**
     (recommended; Unity, Unreal, Godot's "selected scenes").
   - б) Every asset of the project (Godot's "export all"): simpler, larger.
3. **Scripts in a shipped game.**
   - **а) Luau bytecode** (recommended): a faster start, the sources not shipped; the
     shipped game cannot be debugged at a script's line (the editor and `--debug` builds
     still can).
   - б) The sources, as now.
4. **Kinds of builds.**
   - **а) Two from one file:** "Build" (`release`, a minute or two: for testing) and
     "Ship" (`shipping`: fat LTO, longer) (recommended).
   - б) Ship only.
5. **Where and how.**
   - **а) `<game>/builds/<name>/`**: `<Game>.exe`, `data/`, `plugins/`, the notices; what
     did not change is kept between builds; a zip on request (recommended).
   - б) A folder chosen each time, emptied first (Fyrox).
