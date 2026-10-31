# ADR 0126: The one-button game build (E14)

- Status: **Proposed** (2026-10-31). Five questions for the team at the end.
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
| **G3** | the cook from the project: models, textures, colliders, navigation into packs; the player reading a scene's assets from packs |
| **G4** | the editor's Build: the settings section, the task, errors at their causes, Run the Build |
| **G5** | scripts as Luau bytecode, the PSO warm-up list, a size report, an optional zip |

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
