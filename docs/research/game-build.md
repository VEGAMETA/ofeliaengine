# The one-button game build (E14): how engines ship a game

Research for [ADR 0126](../adr/0126-game-build.md). Checked 2026-10-31. No code was carried
over from any of them; the engines below are read as products (and Fyrox's and Godot's
documentation as text).

## Sources

| Engine | Source | License | Checked |
|---|---|---|---|
| Godot 4.4 | [Exporting projects](https://docs.godotengine.org/en/4.4/tutorials/export/exporting_projects.html), [EditorExportPreset](https://docs.godotengine.org/es/4.x/classes/class_editorexportpreset.html) | MIT | 2026-10-31 |
| Unity 6.0–6.3 | [Build Profiles reference](https://docs.unity3d.com/6/Documentation/Manual/build-profiles-reference.html), [scene list](https://docs.unity3d.com/Manual/build-profile-scene-list.html), [create a build profile](https://docs.unity3d.com/6/Documentation/Manual/create-build-profile.html) | proprietary (read as a product) | 2026-10-31 |
| Unreal 5.4–5.5 | [UProjectPackagingSettings](https://dev.epicgames.com/documentation/unreal-engine/API/Developer/DeveloperToolSettings/UProjectPackagingSettings), [IoStoreUtilities](https://dev.epicgames.com/documentation/unreal-engine/API/Developer/IoStoreUtilities?application_version=5.5), [reducing packaged game size (4.27)](https://dev.epicgames.com/documentation/ja-jp/unreal-engine/reducing-packaged-game-size?application_version=4.27), forum threads on IoStore ([1](https://forums.unrealengine.com/t/undocumented-use-io-store-packaging-setting/144296), [2](https://forums.unrealengine.com/t/external-pak-files-and-use-io-store-option/2137290)) | source-available EULA (read as a product, no code) | 2026-10-31 |
| Fyrox 1.0 | [Shipping on PC](https://fyrox-book.github.io/shipping/pc.html), [CI/CD](https://fyrox-book.github.io/shipping/ci_cd.html), [fyrox-build-tools export](https://docs.rs/crate/fyrox-build-tools/1.0.1/source/src/export/pc.rs) | MIT | 2026-10-31 |
| Bevy | no exporter: `cargo build --release` and the game's `assets/` folder beside it (the common practice) | MIT/Apache | 2026-10-31 |

## How each one does it

- **Godot** exports from **presets** kept in `export_presets.cfg` (committed; passwords and
  keys stay in `.godot/export_credentials.cfg`). A preset picks:
  - what goes in: every resource, the selected scenes with what they depend on, the
    selected resources with theirs, or all but the checked ones; filters add non-resource
    files (`*.json`, `*.txt`); dot-files and dot-folders are never exported;
  - the data as one **PCK** (or ZIP) beside the runtime, or embedded in it; a PCK alone
    can be exported for a runtime shipped before (`godot --export-pack "Windows Desktop"`).
  - The runtime is an **export template**, a prebuilt engine binary: the game's scripts are
    data.
- **Unity 6** builds from **Build Profiles**: a platform, a **scene list** (in order; the
  first loads first; a checkbox leaves one out; a profile may override the global list)
  and player settings. What the scenes reference goes in; anything else only through
  Resources folders or Addressables.
- **Unreal 5** packages in stages, **build → cook → stage → package → archive**:
  - the cook turns editor assets into the platform's forms (through the DDC), only for the
    maps listed and what they reference (plus directories "always cooked");
  - packaging writes **IoStore** containers (`.utoc`/`.ucas`, addressed by package path, no
    files inside) beside a small `.pak` for the rest; `bShareMaterialShaderCode` keeps each
    shader once in a library instead of inside each material (smaller, can load slower);
    `bSkipEditorContent` leaves editor folders out; the PSO cache recorded in play is
    bundled for warm-up.
  - Its own documentation of IoStore is thin; the forum fills the gaps.
- **Fyrox 1.0**: *File → Export Project*: a platform, the data folders, extensions left out;
  the output folder is emptied first; it builds the executor in release and copies the
  assets. The same in an `export-cli` crate per project for CI, with
  `--include_used_assets` (only what the resource manager sees used: assets loaded from
  code by path are missed).
- **Bevy** has none: a release build and the `assets/` folder copied by hand or a script.

## What works, what annoys

- **Works:** a settings file committed with the game (Godot's presets, Unity's profiles); a
  start scene and a scene list as the roots of what goes in (Unity, Godot, Unreal's maps);
  "only what is used" plus folders "always in" for what code loads by name (Unreal,
  Fyrox's caveat); the editor and the command line running the same code (Fyrox, Godot,
  Unreal's BuildCookRun); data cooked to the runtime's own forms (Unreal, Godot's
  imported resources); the shader code once, precompiled, with the PSOs recorded in play
  for warm-up (Unreal).
- **Annoys:** Unreal's packaging is long and its failures land in a log far from their
  causes; Fyrox wipes the output folder and copies everything unless asked; Bevy leaves it
  to the game; Godot needs the matching export templates downloaded per version.

## What we take

- A **build settings file** in the game's folder, committed, text (the project's grammar):
  the start scene and the scene list, folders always included, the profile, the output.
- **Roots and references:** the scenes listed and every asset they reach by id (ADR 0108),
  plus the folders always in (for what scripts load by path).
- **One code path** for the editor's button and `ofeliaengine build` (and its JSON output
  for the Claude Code layer, 5a).
- **Cooked data** (our packs, ADR 0056/0076: meshes, BC7 textures, colliders, navigation,
  precompiled SPIR-V for the materials' types and graphs, PSO records) rather than sources:
  the player never parses glTF nor loads Slang in a shipped game (`engine_shader`'s rule:
  games ship SPIR-V).
- **Errors next to their causes**: each failure names the asset or the line, as the
  editor's Output does (E6-2).

## What we do not take

- Prebuilt runtime templates (Godot): a game is a Rust program built with its code.
- A separate stage/archive step (Unreal): the build writes the folder a player runs.
- Wiping the output folder (Fyrox): what did not change is kept (the cook's cache).
