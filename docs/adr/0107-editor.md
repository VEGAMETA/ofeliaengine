# 0107. The editor: linked into the game's build, Play on a copy of the world, every action a command, built in phases

- Status: Accepted (the owner's choices, 2026-10-17)
- Date: 2026-10-17
- Related:
  - CLAUDE.md §12 (the editor), §10 (scripting), §5 (budgets);
  - [ADR 0022](0022-reflection.md) reflection, [ADR 0029](0029-scene-format.md) the scene
    format, [ADR 0030](0030-ui-core.md) the UI core, [ADR 0031](0031-undo.md) undo,
    [ADR 0032](0032-scripting.md) Luau, [ADR 0099](0099-player-crate.md) the player,
    [ADR 0104](0104-save-games.md) saves (the world's copy);
  - [ADR 0108](0108-scene-format-2.md): scene format 2, asset ids, prefab instances;
  - research: `docs/research/editor-architecture.md`; UX notes in `docs/research/ux/`:
    `editor-layout.md`, `command-palette.md`, `viewport-navigation.md`, `scene-tree.md`,
    `inspector.md`, `viewport-tools.md`, `play-mode.md`, `prefabs.md`,
    `asset-browser.md`, `code-editor.md`, `script-debugger.md`;
  - queue item 9 (`docs/status.md`) and 5a (the layer for Claude Code).

## Context

**What exists:**
- The editor's skeleton (`engine_editor`, 3,900 lines):
  - a window with the team's Godot layout, panels showing demo data;
  - the command palette and rebindable shortcuts (`Commands`, `Keymap`);
  - undo over the world (`History`, ADR 0031);
  - an orbit camera and HDR output;
  - its own window and device setup (`src/bin/editor.rs`), separate from the player's.
  - Cold start 0.39 s; 22 µs of UI and 25 µs of painting per frame.
- `engine_ui` (ADR 0030), and `engine_scene`'s format version 1 (plain data only).
- Reflection with containers and the program's list of reflected types (ADR 0022's
  addendum).
- The player (`engine_app`) runs a game's world with every subsystem; saves copy a world
  in the frame (10,000 entities: 0.023 ms).
- The Luau VM with modules and hot reload (ADR 0032). It has no bindings to the engine yet.

**What a game developer lacks:**
- panels bound to the scene, and the game's own code inside the editor;
- Play, and assets with a stable identity;
- prefabs, editing and debugging code;
- tools for every subsystem.

**The owner's choices, 2026-10-17:**
1. **Scope:** the whole §12 list, not only a first wave.
2. **The editor is linked into the game's build** (option А). A separate process comes
   later (option В). The 2.5 s rebuild is accepted for now ("долго конечно но давай
   попробуем").
3. **Prefabs** are in the first version.
4. **Tools:** materials, sound, animation and 2D first, then everything else the engine
   has. "Главное чтобы для разработчика было удобно."
5. **A built-in code editor** at Godot's or Roblox Studio's level (option Б), with VS Code
   kept working beside it.
6. **Asset ids:** the mix (ADR 0108).

## Options

Where the game's Rust code runs, as put to the owner:

1. **Linked into the game's build** (Fyrox's editor, Unreal). The game's components and
   systems are native in the editor, and Play starts at once.
   - An edit of Rust code needs a rebuild (2.5 s for the quick start's game, release) and
     a restart.
   - A crash in game code takes the editor down.
   - **Chosen.**
2. **The game as a dynamic library** loaded by an editor executable. Code reloads without
   a restart, but Rust has no stable ABI: reflection tables and the type list would have to
   be rebuilt across reloads, and types must keep their shapes.
3. **The game in a separate process,** the editor talking to it (Godot, Defold). Crash
   isolation, but everything the editor sees of the game goes through a protocol, and Play
   starts slower. Kept as a later mode for hunting crashes.

## Decision

### 1. The editor is a library linked into the game

- **The game's crate has an `editor` feature** that turns on an optional dependency on
  `engine_editor`.
  - The game's `main` comes from a macro of the player's crate. The macro expands inside
    the game's crate, so `cfg(feature = "editor")` is the game's own.
  - With the feature, `main` runs the editor around the game; without it, the player.
  - `ofeliaengine new` writes the feature. `ofeliaengine edit` runs the game with it.
  - A shipped game does not link the editor: it costs nothing there.
- **The editor runs the game's world through the player's machinery,** so a scene looks
  in the editor exactly as in the game. The player is split so the editor can drive it:
  - a world extracted and drawn without the game's ticks;
  - the scene drawn into a view inside the editor's window;
  - a second window for the game during Play.

  The skeleton's own window and device setup gives way to the player's.
- **A game is made from its loaded scene.** The game gives a constructor, `start`, called
  once its scene is in the world: by the player at launch, by the editor at every Play.
  Scenes come from files edited in the editor. Code that builds a scene by hand still
  works.
- **A code change means a rebuild and a restart:**
  1. The editor watches the game's `src/` and `Cargo.toml` and builds in the background.
     The output goes to the task panel, and errors are linked to their lines.
  2. When the build succeeds, it offers a restart, or restarts by itself if set to.
  3. The session is saved: open documents with their unsaved changes, cameras, selection,
     layout and panels.
  4. The new executable starts and brings it all back.
  - **Target:** under 1 s from a finished build to working again (cold start is 0.39 s
    today).
  - Whether the editor builds with the dev profile (the game at `opt-level` 1,
    incremental) or release is measured in E1. Dev should rebuild faster than release's
    2.5 s.
  - Undo history does not survive a restart in this version: component layouts may
    change between builds.
- **Later:**
  - hot patching of function bodies (Subsecond-style, CLAUDE.md §10), so most edits skip
    the restart;
  - the separate-process mode for Play (the owner's В).
- **Crashes:**
  - A panic in the game's hooks during Play is caught at the hook's boundary
    (`catch_unwind`). Play stops, the panic is shown at its line, and the play world is
    dropped; the editor stays. Development profiles unwind; only `shipping` aborts.
  - A crash of the process (native code, a driver) brings the session back on the next
    start (§4).

### 2. Worlds

- **The edit world.** Each open scene has one. It is the truth for its file and changes
  only through commands inside `History` transactions (ADR 0031).
- **The play world** is made when Play starts:
  - the edit world is copied through the saves' capture and apply (ADR 0104);
  - the game's `start` builds the game object;
  - the game's hooks then run on the copy.
- **While playing,** the tree, the inspector and the viewport show the play world.
  - Edits there apply at once and are tracked against the edit world by scene id.
  - **"Keep changes"** copies chosen fields, entities, or everything changed into the edit
    world as one undo step. Entities the game spawned can be kept as new ones.
- **Stop** drops the play world and the game object, and the edit world is shown again.
- **On the GPU,** each world on screen has its own runtime (extraction and GPU scene).
  Meshes, materials and textures are shared. The memory this costs is measured in E3.

### 3. Every action is a command

- **Every editor action is a command in `Commands`:** an id, a title, a category, default
  key chords per key map, a context, a "can run" check, and a run function with typed
  arguments.
- **One registry feeds everything:**
  - the palette, menus, tool bars and shortcuts (rebindable, conflicts shown);
  - the automation interface: an MCP server and the command line's JSON output (queue
    5a). An agent can do whatever a person can.
- **A command that changes the world** runs inside one `History` transaction.
- **Commands keep their arguments,** so "Repeat last" (Shift+R) and the "last operation"
  panel (Blender's "Adjust Last Operation") run them again with changed values.
- **Selection is not an undo step by itself.** Each step stores the selection before and
  after, and undo and redo restore it.

### 4. Documents, saving, sessions

- **Tabs:** scenes and scripts share one tab row; a script opens in the code editor. A
  prefab opens as a scene.
- **One history across tabs** (ADR 0031). Undoing a step made in another tab switches to
  that tab.
- **Scene files** are format 2 (ADR 0108).
- **The project's local folder `.ofelia/`** holds the asset cache, autosaves and the
  session. It is never committed: `ofeliaengine new` ignores it.
- **Autosave:**
  - after 30 s with unsaved changes, and before Play and before a restart;
  - into `.ofelia/`, never over the user's files;
  - after a crash or a restart, the session comes back without a dialog (VS Code's "hot
    exit");
  - the user's file changes only on Ctrl+S.

### 5. Assets

- **Identity and references:** ADR 0108 (the owner's mix).
- **The local cache** in `.ofelia/` holds:
  - the index of ids and paths, and where each asset is used;
  - import states, thumbnails, cooked data.

  It is rebuilt from the files at any time. Its form (our own files or SQLite) is chosen
  by measurement when it is built (E4).
- **Import is automatic:**
  - a changed or new file is cooked into the cache in the background;
  - its state and errors show on the asset;
  - the new version is swapped into the edit and play worlds.

### 6. The code editor and the debugger

- **Built into the editor.** VS Code stays first-class, with the same `.d.luau` types and
  the same debugger over DAP.
- **The smart features** come from Luau's own analysis library (MIT): diagnostics,
  completion, hover, definition, references.
  - It is the `Analysis` sources of the same Luau release as the VM, which `luau0-src`
    does not include. They are vendored under `third_party/` and built with `cc`, like
    Jolt.
  - It is called in process, off the UI thread.
  - Its build time and size are measured before it lands (E8). The fallback is luau-lsp
    (MIT) as a child process.
- **Text editing:** one core for every text field: one line in the inspector and the
  palette, many in the code editor. It handles cursors (several at once), selection, undo,
  the clipboard and IME. Whether the buffer is a rope or a gap buffer is chosen by
  measurement (E8).
- **The debugger:**
  - **the core** is in `engine_script`, over Luau's debug interface: breakpoints with
    conditions, hit counts and logpoints; stepping; the stack; locals and upvalues read
    and written; watch expressions; pause on errors;
  - a DAP server on localhost serves VS Code;
  - the editor's panel and gutter call the core directly.
- **Pausing:**
  - Scripts are entered as resumable Luau threads. A breakpoint suspends the script
    (`lua_break`): the game's ticks stop and the editor keeps running.
  - Where a Rust call sits between two scripts on the stack, Luau cannot suspend. There
    the game thread waits inside the breakpoint while the editor draws its frames from
    within it. In that case the world is read-only in the editor, because the paused Rust
    call may hold borrows of it.
- **Cost:** nothing without breakpoints. A function with one runs interpreted until it is
  reloaded (Luau's native code has no breakpoints).

### 7. Phases

Each phase starts with its UX note (§12), then code, tests, measurements and a PR.

| Phase | What |
|---|---|
| **E1. Foundation** | the editor linked into a game; the player split so the editor drives it; scene documents in format 2 (names, strings, asset references); the scene tree and the inspector on reflection (multi-editing, filter, reset, scrubbing, expressions); selection; every action a command; autosave and the session; rebuild and restart; one-line text fields |
| **E2. Viewport tools** | picking from an id buffer, outlines, gizmos, snapping, keyboard transforms, placement on surfaces by drag and drop, duplicate, frame, bookmarks; overlays through debug draw channels (lights, cameras, sound ranges, colliders, the navigation mesh) |
| **E2b. Blockout** (the owner, 2026-10-17) | brushes for greyboxing a level, like Godot's CSG: box, wedge and ramp, stairs, cylinder, sphere; add, subtract, intersect; face editing (extrude a face, move an edge); colliders and the navigation mesh made from them, so Play runs on the greybox at once; a grid material in world space; bake to a mesh. Our own Rust implementation over convex brushes held as planes (the Quake, Hammer and TrenchBroom way), not Godot's C++ library |
| **E3. Play** | the play world, the game's window (embedded as an option), live edits, "Keep changes", pause and step, play from here, restart; entering ≤ 1 s, measured |
| **E4. Assets** | ids and the local cache (ADR 0108), background import, the file browser with rendered thumbnails, drag and drop, find references, import settings in the inspector |
| **E5. Prefabs** | instances, overrides shown in the inspector, revert and apply, nesting, variants, make and unpack (ADR 0108) |
| **E6. Output** | the console with log filters, errors linked to their causes, the task panel, the history panel, the profiler's first panel |
| **E7. Luau in the game** | bindings generated from reflection, `.d.luau`, a script component and its hooks, hot reload in the editor. The public scripting API goes to the owner (CLAUDE.md §3) |
| **E8. Code editor** | the multi-line core, highlighting, Luau's analysis, find and replace, find in files |
| **E9. Debugger** | the core, DAP for VS Code, the panel and the gutter |
| **E10. Materials** | graph and code (Slang) with a live preview, instances, previews in the browser (CLAUDE.md §9) |
| **E11. Sound** | the mixer panel (buses, sends, meters, effects), previews, sources' and zones' gizmos; VST3 plugins' windows later |
| **E12. Animation** | clip preview, the skeleton, the timeline and keys, state machines, blending |
| **E13. 2D** | tile maps (painting, rules), sprites (slicing, atlases), the 2D view's tools, 2D physics shapes |
| **E14. The rest the engine has** | navigation (bake settings, test paths), physics (layers, joints), lighting (probes, the GI volume), post-processing and camera settings, video, localization tables, save files, networking (co-op in one process, the network simulator), the input map, the profiler panel with "Open in Tracy", the one-button build |

5a, the layer for Claude Code, grows from E1's commands: the MCP server comes with E1 and
gains each phase's commands.

### 8. Budgets

| What | Target |
|---|---|
| Cold start (an empty project) | ≤ 2 s (0.39 s today) |
| Entering Play | ≤ 1 s |
| Back to work after a finished build | ≤ 1 s |
| Input answered | within the frame |
| An idle editor | builds no UI frame (ADR 0030) |
| UI per frame, typical layouts | ≤ 1 ms |
| Allocations per frame, steady state | 0, panels included |

### 9. Tests

- Panels run without a GPU (`engine_ui` frames) under scripted input.
- The command layer drives end-to-end tests: open a scene, run commands, compare the
  scene's text with what is expected.
- Offscreen shots cover the viewport's tools.
- The zero-allocation test covers the editor's steady state.

## As built (E1-3, 2026-10-17)

The editor runs on the player.

- **`engine_app::Overlay`:** an overlay adds its own passes to the frame's graph over the
  finished frame, after the game's UI and before presentation
  (`Player::frame_overlaid`, `capture_overlaid`).
- **`Player::set_view_rect`:** the scene is drawn into a part of the output. The frame
  renderer's view target already took a rectangle (split screen). The player gives it
  the rectangle, sizes the view from it and takes the camera's aspect from it.
- **`engine_editor::run`, `run_frames` and `editor`:**
  - `EditorGame` wraps the game: it takes the window's events and builds the shell in
    the frame's presentation step, from the world; in edit mode nothing of the game
    runs;
  - `EditorOverlay` paints the shell with its own 2D renderer and glyph texture;
  - the editor's camera is an entity without a scene id, above every camera of the
    scene;
  - the viewport panel's rectangle sets the next frame's view.
- **The tree and the inspector read the world:** the scene's entities in their hierarchy,
  and the selected entity's components written as scene lines (`engine_scene::write_lines`),
  assets by path. Editing is E1-4.
- **The editor keeps Windows' power throttling** (`process_setup` off), as the team
  decided.
- **The editor binary** opens a scene file or an empty world. Its own device and window
  set-up went away with the demo data.
- **Lost for now: HDR output.** The old editor presented in HDR, and the player presents
  in SDR. HDR output in the player comes next, for games and the editor alike.
- **Measured** on ref-1 (release, 1600×900): start and 60 frames in 0.51–0.69 s.
- **Tested** by `engine_editor` `app`: a scene file opened offscreen. The red cube is
  drawn inside the viewport panel and nowhere outside it, and selecting it from code
  selects it.

## Consequences

- **Games change shape:** data lives in scenes edited in the editor, logic in hooks and
  systems over components, and a game builds itself from its loaded scene (`start`). The
  quick start and `ofeliaengine new` move to this shape in E1.
- **The player is split into parts the editor drives** (E1). The skeleton's separate
  device setup goes away.
- **A shipped game costs nothing more:** the editor is not linked into it.
- **A crash in native code** (C++ physics, a driver) still takes the editor down. The
  session comes back; the separate-process mode comes later.
- **Undo history is lost on a restart** in this version.
- **Risks, each measured in its phase:**
  - two worlds on the GPU during Play;
  - the game window's pacing beside the editor's;
  - the analysis library's build time;
  - the read-only world under a breakpoint that cannot suspend.
