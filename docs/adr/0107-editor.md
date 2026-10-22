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
- **Commands keep their arguments,** so "Repeat last" and the "last operation" panel
  (Blender's "Adjust Last Operation") run them again with changed values. Its key comes
  with it: Shift+R, Blender's, picks the rotate gizmo's handles since E2-2b.
- **Selection is not an undo step by itself.** Each step stores the selection before and
  after, and undo and redo restore it.

### 4. Documents, saving, sessions

- **Tabs:** scenes and scripts share one tab row; a script opens in the code editor. A
  prefab opens as a scene.
- **One history across tabs** (ADR 0031). Undoing a step made in another tab switches to
  that tab.
- **Scene files** are ADR 0108's format (version 3 since 2026-10-20: the roots' order).
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
| **E1. Foundation** | the editor linked into a game; the player split so the editor drives it; scene documents in format 2 (names, strings, asset references); the scene tree and the inspector on reflection (multi-editing, filter, reset, scrubbing, expressions); selection; every action a command; autosave and the session; rebuild and restart; one-line text fields; icons by kind, the same in the add menu, the tree and the palette, a model's from its mesh (team decision, 2026-10-17) |
| **E2. Viewport tools** | picking from an id buffer, outlines, gizmos, snapping, keyboard transforms, placement on surfaces by drag and drop, duplicate, frame, bookmarks; overlays through debug draw channels (lights, cameras, sound ranges, colliders, the navigation mesh) |
| **E2b. Blockout** (the owner, 2026-10-17) | brushes for greyboxing a level, like Godot's CSG: box, wedge and ramp, stairs, cylinder, sphere; add, subtract, intersect; face editing (extrude a face, move an edge); colliders and the navigation mesh made from them, so Play runs on the greybox at once; a grid material in world space; bake to a mesh. Our own Rust implementation over convex brushes held as planes (the Quake, Hammer and TrenchBroom way), not Godot's C++ library ([ADR 0110](0110-blockout-brushes.md)) |
| **E3. Play** | the play world, the game's window (embedded as an option), live edits, "Keep changes", pause and step, play from here, restart; entering ≤ 1 s, measured |
| **E4. Assets** | ids and the local cache (ADR 0108), background import, the file browser with rendered thumbnails, drag and drop, find references, import settings in the inspector; asset fields in the inspector as in Godot (team decision, 2026-10-17): a square preview the engine renders with the asset's own materials and shaders, a model's mesh picked from the built-ins or the project or dropped from the file browser, its material slots the same way |
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

## As built (E1-4a, 2026-10-17)

The editor edits the world.

- **Undo records every component scenes save** (ADR 0031 as amended): plain data as bytes,
  the rest (`Name`, `Model`, a game's components with strings and lists) as the lines a
  scene writes for it, read back into a default value
  (`engine_scene::write_lines_with`, `read_lines`). Runtime state (`hidden`, `skip`,
  types without reflection or `Default`) is left to the systems that own it.
- **The inspector edits fields through reflection** (`engine_editor::inspector`):
  - numbers dragged sideways; vectors and small arrays side by side;
  - a quaternion as Euler angles in degrees (Y, X, Z), so a 45° step means 45°;
  - checkboxes; enums from a list (a floating popup);
  - strings, references and containers shown as text until the text field (E1-4b);
  - a transform reads translation, rotation, scale;
  - a drag is one undo step (a merge key per gesture), and Escape takes it back exactly
    (`History::revert`).
- **Snapping, the owner's rules** (`engine_editor::Snapping`): free by default; Shift
  while dragging takes the other mode (steps from the start); Ctrl rounds the value to
  the grid. Steps: position 0.1, scale 0.1, rotation 45°, numbers 0.1 or their own
  `step`. The tool bar's grid button (Y) switches the default. The gizmos (E2) take the
  same rules.
- **Commands with shortcuts** (`Commands`, `Keymap`, physical keys): undo Ctrl+Z, redo
  Ctrl+Y and Ctrl+Shift+Z, save Ctrl+S, delete Delete, duplicate Ctrl+D, add Ctrl+A,
  snapping Y. `EditorGame::run_command` runs one by its stable name, as the palette and the
  MCP layer will.
- **Duplicate** copies a subtree with new scene ids; references into the subtree move to
  the copies (`History::duplicate_subtree`).
- **Add** (the scene dock's `+`): an empty entity, the built-in shapes, a sun, point and
  spot lights, a camera, where the camera looks, on the grid.
- **Saving** writes the scene under its own id (read from the header the player loaded,
  `engine_scene::scene_header`), asset references by path; written beside the file and
  moved over it. A new scene becomes `untitled.scene`. The tab shows `*` while
  `History::head` differs from its value at the last save.
- **`EditorGame::set`** sets a value by its scene path from scene text, one undoable
  step: tests, scripts and the command line.
- **`engine_ui`:** drags measured from the press (`Ui::drag_delta`), `Response::released`,
  Control in `UiInput`, `Ui::hovers`.
- **Measured** on ref-1 (release): the editor's frame on the CPU with a tree of 200
  entities 74 µs, 81 µs with a selection in the inspector. Scenes save and load as fast as
  before (6.60 and 9.65 ms for 10,000 entities).
- **Tested:**
  - without a GPU, the editor's real frames driven by window events: a number dragged
    with Ctrl, then free, then with Shift; Escape; undo and redo by their keys; Ctrl+S
    and the file; a checkbox, an enum's list and a click outside it; add, duplicate,
    delete with undo; saving an untitled scene; values set by path;
  - on the GPU (`app`): duplicate the cube and move the copy on the player, see it drawn,
    undo both, redo and save;
  - undo of components that are not plain data, of duplicated subtrees with inner
    references, and in the randomized walk (`tests/editor/undo.rs`).
- **Left for E1-4b:** text fields (rename, typed values and expressions, strings), the
  command palette, multi-selection and multi-editing, the inspector's filter, reset to
  default. The order of a scene's roots (files sort them by id) is still to settle.

## As built (E1-4b-1, 2026-10-17)

Text fields.

- **`engine_ui::Ui::text_field`:** one field edited at a time, its buffer, caret and
  selection kept in the `Ui`. The layout measures the caret, the selection and a press's
  character, and scrolls the text to keep the caret in view. Typing, Backspace and
  Delete, the arrows (Ctrl: by word), Home, End, Shift to select, Ctrl+A, C, X and V;
  Enter, Tab and a click elsewhere commit, Escape cancels. `edit_field` starts one from
  code, `autofocus` as soon as it is built.
- **`engine_platform::clipboard`:** Windows' clipboard text.
- **The tree:** F2 and a double click rename in place; the arrow opens and closes a row
  (a click on a row only selects it); the filter keeps matching names and their parents.
- **The inspector:** a number clicked without a drag becomes a field; typed values are
  expressions, absolute or relative (`engine_editor::expr`); strings are typed; the filter
  keeps matching fields.
- **Shortcuts wait while a field is typed in** (only Ctrl+S saves).
- **Measured:** the editor's frame with a tree of 200 entities and a selection, 84 µs on
  ref-1's CPU (81 before the fields).
- **Tested** without a GPU through the editor's frames: F2 and typing with Y and Delete
  going to the field, a double click and Escape, expressions (`2*pi`, `+=1`, a refused one),
  a string, both filters; the text field alone in `engine_ui`.

## As built (E1-4b-2, 2026-10-17)

The command palette, reset to default, copying values
(`docs/research/ux/command-palette.md`, `docs/research/ux/inspector.md` §6).

- **The palette** (Ctrl+Shift+P): a floating box over the shell with the best twelve
  matches, the list scrolling with the highlight. Commands show as "Category: Title",
  the matched letters in the accent and the shortcut beside each. The add menu's kinds
  are commands (`scene.add.cube`…). Up and Down move the highlight (the query box leaves
  them to the list: `TextField::search`), Enter or a click runs it, Escape or a click
  elsewhere closes it.
- **`@` finds the scene's objects** (Ctrl+P opens the palette with it): every entity with
  a scene id in tree order, ranked by `engine_editor::Fuzzy`, the commands' matcher made
  usable for any text; the one picked is selected and its parents' rows opened. `>` is
  the commands' prefix; `#` assets and `:` settings come with their panels.
- **Reset:** each component type's `Default` value is made once per session
  (`inspector::Defaults`). A field differing from it is marked changed and gets ↺ in a
  place every row keeps, so fields stay aligned and none moves when it starts to differ.
  Backspace over a hovered field resets it too, as one undo step. A `Name` has no reset.
- **Copy and paste** over a hovered field (`Ui::mark_region`, a row's hover whatever
  widget is under the mouse): Ctrl+C writes the value's scene text (a string as it is),
  Ctrl+V sets it through the scene-path setter, checked against the field's type. The
  editor's `Clipboard` is Windows' or, in tests, its own.
- **Measured** on ref-1's CPU: the frame with 200 entities and a selection 82.5 µs (83.5
  before), 88 µs with the palette open; the `@` search over 10,000 objects 0.62–0.73 ms
  per keystroke, mostly the walk of the tree and the names (the research note's target:
  under 1 ms). The shell's zero-allocation test now runs with the palette open and reset
  buttons shown.
- **Tested** without a GPU through the editor's frames: a command found and run with
  Enter, objects found with `@` and picked with Down and Enter, Escape, a click outside,
  a row clicked; ↺, Backspace, Ctrl+C and Ctrl+V between fields, a refused paste, a name
  copied to another entity. `engine_ui`: the search box and hover regions.
- **Not done:** the tree does not scroll to an object picked in the palette; copying a
  vector's single part (Blender's Ctrl+C on a part); component headers' copy, paste and
  reset (`inspector.md` §7).

## As built (E1-4b-3a, 2026-10-20)

The selection in the scene tree, icons by kind (`docs/research/ux/scene-tree.md` §4).

- **The selection** is a list in the order picked; the last is the active entity, which
  the inspector shows (its header counts the rest) and F2 renames. A click selects one,
  Ctrl+click adds or takes out, Shift+click selects the tree's rows from the anchor (the
  last plain or Ctrl click), Ctrl+Shift+click adds that range. The active row is drawn
  brighter than the others selected.
- **The tree's keys** are commands bound in the tree's key context (`KeyContext::Panel`),
  live after a press in the tree: Up and Down move, Shift extends, Left closes or goes to
  the parent, Right opens or goes to the first child, Shift+Right opens the branch.
  They show in the palette and are rebindable like every command.
- **Delete and Ctrl+D act on the selection's roots** (a child goes with its selected
  parent), as one undo step. A copy comes right after its original and is numbered among
  its siblings ("Cube 2"); so is a new entity among the roots. Undo and redo select again
  what was selected, by scene ids.
- **`engine_ui::Ui::reveal`** scrolls the scroll containers around a node just enough to
  show it, from the next frame: the row the keys moved to, an object picked with `@`.
- **Icons by kind** (team decision, 2026-10-17): one icon per kind in the add menu, the
  tree and the palette (objects, and the "Add …" commands); a model's is its built-in
  shape's (cube, sphere, plane, cylinder) or a model's; the spot light has its own. An
  icon takes a column of one width, so labels line up.
- **Measured** on ref-1's CPU, release, 200 entities: the frame 83–84 µs with one selected
  (82.5 before), 92 µs with all 200 selected. An icon is one text node of fixed width;
  a container around each icon had cost 9 µs.
- **Tested** through the editor's frames: Ctrl+, Shift+ and plain clicks, the header's
  count, the arrows with and without Shift, the arrows going elsewhere after a click in
  the viewport, Left and Right on a parent and a leaf, the selection duplicated and
  deleted as one step and undone, numbered names, a picked row scrolled into view, the
  kinds' icons in the menu and the tree; `engine_ui`: reveal.
- **Not done:** Ctrl+G (group under a new entity), multi-editing in the inspector
  (E1-4b-3b).

## As built (E1-4b-3b, 2026-10-20)

Multi-editing in the inspector (`docs/research/ux/inspector.md` §4, §14).

- **What is shown:** the inspector is built from the active entity, then
  `Inspector::compare` keeps the components every selected entity has and marks the parts
  where another holds a different value (`Property::mixed`, a bit per part), shown "—".
  Plain values are compared by their bytes part by part (a rotation as a whole), strings
  and containers by their text. A field is changed (↺) when any entity's value differs
  from the default. A checkbox that differs shows a dash.
- **What an edit does**, to every selected entity with the component, as one undo step:
  - a typed value is an expression worked out per entity, so an absolute one sets them
    all and a relative one (`+=1`, `*2`) moves each from its own; "—" kept as it was
    changes nothing;
  - a drag moves each from its own start (the owner's snapping rules apply to each);
  - a checkbox turns all off when all are on, otherwise all on; a choice, a reset and a
    paste go to all; strings are set on all through the scene-path setter (`set_on`: the
    entity index built once, nothing changed when one refuses the text).
- **Measured** on ref-1's CPU, release, 200 entities: the frame 84 µs with one selected,
  105 µs with all 200 selected (the comparison about 63 ns per entity: one component
  lookup per entity and section, then the properties' bytes).
- **Tested** through the editor's frames: shared components and dashes (a part, a name),
  an absolute and a relative typed value, a drag from each start, a differing checkbox,
  a reset and a paste on two entities, one undo step each.
- **Not done:** the copy of a component to the selection (`inspector.md` §6, "Copy to
  selected"), header menus (§7).

## As built (E1-5a, 2026-10-20)

The game's `main` from a macro, `ofeliaengine edit`, the quick start as a scene.

- **`engine_app::main!(settings, start)`** writes the game's `main`: `settings` gives the
  player's settings, `start` (`engine_app::Start`) makes the game once its scene is in
  the world. The macro expands in the game's crate, so `cfg(feature = "editor")` is the
  game's own: with it, `engine_editor::run`; without it, `engine_app::run` (the player
  made, its scene loaded, `start`, the frames until the window closes). A crate without
  an `editor` feature (the engine's own examples) gets no `unexpected_cfgs` warning: the
  macro allows it.
- **`ofeliaengine new`** writes:
  - the optional `engine_editor` dependency and the `editor` feature;
  - the quick start's scene (`scenes/main.scene`) and its two materials, under fresh
    asset ids, with the scene's references rewritten to them, so two games from the
    template share no asset id;
  - `.ofelia/` in `.gitignore` (§4).
- **`ofeliaengine edit [DIR]`** ran `cargo run --release --features editor` in the
  game's folder, after checking that its manifest declares the feature; otherwise an
  error says what to add. It stays on release until E1-5c measures the dev profile
  against it.
- **The quick start is a scene:**
  - a floor, a cube, the sun and a camera in `examples/quick_start/scenes/main.scene`
    (version 3), with `.material` files;
  - the code finds the cube and the camera by name (`engine_scene::find_named`, in the
    prelude) and keeps the scene camera's lens, with the player's antialiasing for the
    GPU;
  - the test's image is the guide's (`docs/dev-guide/quick-start.png`), unchanged.
- **Tested:**
  - `new` writes the feature, and the scene and materials under ids that are not the
    template's (two games get different ones); the scene names the materials by their
    new ids;
  - `edit` finds the feature in `[features]` only, and builds cargo's arguments;
  - the editor opens the quick start offscreen on the game's own `settings` and `start`,
    with its four entities and both materials found. The test was checked to fail with a
    material missing.
  - A game made by `new` outside the repository checks with and without the feature, in
    its own target. The first check took 49 s, the engine's C and C++ included; the
    editor added 5 s.
- **Not done:**
  - crash reports and logging from `main!` (a game that wants crash reports writes its
    `main` by hand, `player.md`);
  - autosave and the session (E1-5b);
  - rebuild and restart (E1-5c).

## As built (E1-5b-1, 2026-10-20)

Autosave and the unsaved changes brought back (§4; UX note `docs/research/ux/sessions.md`,
whose open questions were settled by its recommendations, team decision 2026-10-20).

- **`.ofelia/` in the project:**
  - `session.lock`, held open without sharing while an editor runs: a second editor on
    the project is refused, and a lock found at start that nobody holds means the last
    run did not exit cleanly;
  - `session`, the session file in the scene grammar (`[session version=1]`, a
    `[document]` block per scene): the scene's path and id, what its file was (size,
    last write time, a 64-bit hash) and its autosave;
  - `autosave/<stem>-<scene id>.scene` and two older copies (`.1`, `.2`), each the text
    Ctrl+S would write.
- **When:**
  - the first change not in an autosave sets a deadline 30 s on; later changes do not
    move it;
  - the write waits for a quiet moment (no drag, no typing, no button held, a second
    without input), at the latest 30 s past the deadline;
  - `EditorGame::save_session` writes at once and waits for the disk (before Play and
    before a restart);
  - once the scene is clean again, its autosaves go.
- **Writing:** the text is made on the editor's thread; one writer thread writes every
  file whole (beside it, flushed, renamed over it, a refused rename retried) and moves
  the older copies down when the newest is at least 30 s old.
- **At start** (`EditorGame::begin_session`, called by `run`): the lock is taken, and the
  scene's autosave, if the session lists one, replaces the loaded scene in the world;
  the tab shows `*` and the status bar says from when, and "after a crash" when the lock
  was left behind.
- **At a clean exit** (`EditorGame::end_session`): the last changes autosaved, every write
  on the disk, the lock gone. Nothing is asked (VS Code's hot exit).
- **The file changed on disk** since the changes were made from it: the changes come back
  marked (a warning sign on the tab, a note in the status bar). Ctrl+S refuses; "Take Disk
  Version" loads the file as one undo step and keeps the autosave as `….conflict.scene`;
  "Save Over Disk Version" writes the changes over it. A file gone from the disk is
  written again by Ctrl+S.
- **A start that crashes while it brings changes back:** the lock says `restoring` for
  its first 10 s, and a start that finds it so opens the file as saved, saying where the
  changes are kept.
- **Measured** on ref-1, release, entities with a name, a transform and a model:
  - 200 entities: the text 0.15 ms, loading it back 0.41 ms;
  - 10,000: 8.9 ms and 19 ms;
  - the file's hash 0.58 ms at 10,000;
  - the per-frame cost is a few comparisons, without allocating.
- **Tested** (`engine_editor`, without a GPU):
  - changes back after a crash and after a clean exit;
  - a second editor refused;
  - Ctrl+S writes, and the autosave goes;
  - a file changed on disk: Ctrl+S refused, the disk version taken and undone, the
    changes saved over it;
  - the deadline and the quiet moment;
  - autosaves gone once the scene is clean;
  - a start that stopped while restoring;
  - the lock, the session file, the base and the writer, each on its own.
- **Not done:**
  - the view in the session: cameras, selection, folded rows, panels, recent commands
    (E1-5b-2);
  - "Restore Unsaved Changes…" with the older autosaves (E1-5b-2);
  - the text of a 10,000-entity scene (8.9 ms) made behind the frame from a copy of the
    world, as save games do (ADR 0104);
  - Ctrl+S through the writer;
  - files watched while the editor runs (E4).
- **For E1-5c:** a restarted editor waits for the old one's lock instead of being
  refused.

## As built (E1-5b-2, 2026-10-20)

The view in the session, and older autosaves brought back.

- **The session file** gains an `[editor]` block and a view per `[document]`. Keys and
  sections it does not know are skipped, so a later editor's file still reads.
  - `[editor]`: snapping by default and its steps; the last 50 commands run, by their
    stable names.
  - per scene: the orbit camera (target, yaw, pitch, distance); the selection by scene
    id in pick order, and the anchor; the tree's closed rows (sorted, so an unchanged set
    writes the same text); the tree's and the inspector's filters; the tool.
- **Written** when its text changed: with every autosave, at the exit, and otherwise at
  most every 10 s at a quiet moment.
- **Back at start:** the camera, the selection (entities gone are left out), the closed
  rows, the filters, the tool, snapping, and the palette's ranking of recent commands
  (`Commands::recent`).
- **"Scene: Restore Unsaved Changes…"** opens the command palette as a list: the scene's
  latest autosave, the two older ones and the one kept after a conflict, the newest first,
  named by their age. The query narrows the list. The pick replaces the scene as one undo
  step, as "Take Disk Version" does (both through one replacement keyed by scene ids).
- **Not kept:** panel sizes and open bottom tabs (the shell's panels are fixed for now);
  undo history (§1); popups.
- **Tested:** the view across a clean exit and a start (camera, selection and anchor,
  closed rows, filters, tool, snapping, recent commands); the session file's new blocks
  and skipped keys; an older autosave listed, picked and undone.

## As built (E1-5c-1, 2026-10-20)

The game's code rebuilt while the editor runs, and the editor restarted with it (§1;
research `docs/research/rebuild-restart.md`).

**Team decisions, 2026-10-20.** Measured first on ref-1: a change to the quick start's
code to a finished build, with the editor feature, the median of 10.

| Profile | Change → build | First build | PDB |
|---|---|---|---|
| release | 2.95 s | 92 s | 6 MB |
| dev, as `new` wrote it | 2.07 s | 117 s | 170 MB |
| dev, the engine without debug info | 1.48 s | 98 s | 11 MB |

- **The editor builds with the last one** (option В): the game's code at `opt-level` 1
  with line tables, the engine at 3 without debug info. `ofeliaengine new` writes it as
  the dev profile. The game's code runs slower in Play than shipped.
- **A new build restarts the editor by itself** while its window is in the background,
  and is offered (Ctrl+R) while it is in front (option Б). "Editor: Restart Policy…"
  sets always or ask; the session keeps it.

How it works:

- **The entry:** `engine_app::main!` with the feature calls `engine_editor::run_game`
  with the game's crate (`GameCrate`, its manifest folder).
- **The builder,** a thread:
  - it watches the game's folder (`DirectoryWatcher`, recursive), and after a build the
    folder of the sources outside it that cargo's dep-info names (the engine's crates);
  - a change to a source (one the dep-info names, or before it a Rust file or a manifest
    of the game's) settles 100 ms, then `cargo build --quiet --features editor
    --message-format=json` runs below normal priority, without a console window;
  - one build at a time; changes during a build build again after it; cargo's output
    folder and `.ofelia/` are ignored;
  - errors are read by their primary span (file, line, column), link errors without one;
    warnings are counted.
- **A new executable** (not `fresh`) is copied with its PDB into a slot of
  `.ofelia/run/` (`a`, `b`, `c`): not the running one's, and passing over one whose files
  are held.
- **The restart:**
  - the session is saved and flushed (the autosave, the view), the frame loop ends, and
    the session ends, releasing the lock;
  - the new copy starts in the same folder with the same arguments.
    `ENGINE_EDITOR_RESTART` says when; `ENGINE_EDITOR_BUILD` names cargo's path of the
    build, for its dep-info;
  - the old process exits after it. The new editor waits up to 10 s for the lock and says
    how long the restart took.
- **Under `cargo run --features editor`** the editor runs cargo's own file. It copies
  itself into a slot, starts that copy and exits, before its window opens.
- **`ofeliaengine edit`:**
  - builds, with cargo showing its messages, and copies the build into a slot;
  - starts the copy in a job (`engine_platform::process::Job`) and waits until the job is
    empty: the editor and its restarts;
  - a failed build starts the newest slot's.
- The editor's messages go to stderr and to
  `%LOCALAPPDATA%/ofeliaengine/logs/<game>-editor.log`.
- **Measured** on ref-1, a game made by `new` outside the repository, its window
  offscreen:
  - a change to its code rebuilt in the editor in 1.5–1.6 s;
  - the restart, from the old editor starting the new one to the new one's first frame,
    took 0.36–0.37 s.
- **Tested:**
  - a new build restarts by itself in the background, is offered in front and taken by
    Ctrl+R, and is never taken by itself under "ask";
  - the session is saved before the restart;
  - a failed build's errors are shown by line and listed;
  - the policy is picked in the palette and kept;
  - the builder's dep-info, its watched folders, which changes build, cargo's JSON lines,
    the slots;
  - the launcher's arguments, executable, slot and last good build;
  - the job counts a process and its children;
  - end to end: the game made by `new` launched by `ofeliaengine edit`, its code changed,
    rebuilt and restarted by itself.
- **Not done:**
  - the restart without a gap (E1-5c-2): the new window cloaked until its first frame at
    the old window's place, and the old editor kept until then and back if the new build
    fails at start;
  - the editor's frame time during a build, measured;
  - a build that finishes during Play (E3);
  - a Dev Drive for game targets (a machine setting).

## As built (E1-5c-2, 2026-10-20)

The restart without a gap, and the window where it was.

- **The window's place** (`engine_platform::window::WindowPlacement`) is the restored
  rectangle and whether the window is maximized, as `GetWindowPlacement` gives it:
  - `WindowDesc::placement` opens a window there (`SetWindowPlacement`; Windows moves one
    off every monitor back onto one);
  - the player has `PlayerSettings::window_placement` and `Player::window_placement`;
  - the editor reads its window's place every 30 frames and at the exit, and the session
    keeps it (`[editor] window`);
  - every start opens there, the session's place read before the window is made.
- **Hidden until drawn:** `WindowDesc::cloaked` and `PlayerSettings::start_hidden` make
  the window cloaked (`DWMWA_CLOAK`: composed, not shown); `Player::show_window` uncloaks
  it and can bring it to the front.
- **The handover:**
  1. The old editor saves and ends its session.
  2. It starts the new build with `ENGINE_EDITOR_RESTART` set to `<when> <its process
     id> <in front>`, and lets it take the foreground (`AllowSetForegroundWindow`) when
     its window had it.
  3. The new editor opens hidden at the old window's place. After its second frame it
     shows itself, in front if asked, and writes `.ofelia/run/up-<old process id>`.
  4. The old editor waits for that file, polling every 5 ms, with its window still on the
     screen. Then it exits.
- **When the new build fails at its start:**
  - it may exit first, or show nothing within 20 s; in the second case it is ended;
  - the old editor takes its session back (`resume_session`: the lock again, a new
    writer, its autosave and conflict as they were) and goes on;
  - the status bar says why until the next build.
- **Measured** on ref-1, a game made by `new`, its window offscreen:
  - the old editor waited 0.37–0.41 s from starting the new build to the signal;
  - the new editor reached its first frame in 0.34–0.38 s.
- **Tested:**
  - the restart's variable read, the older form too;
  - the wait: a start that exits is a failure with its exit code, a signal is a success,
    and the signal file is taken;
  - a failed restart goes on with its session, the lock held again;
  - a window opens at its placement, cloaked, and is uncloaked by command;
  - the session keeps the window's place.
  - End to end: a code change handed over, a new build that panicked at its start left
    the old editor running with the reason, and the next build was handed over.
- **Not done:**
  - the swap watched on a real screen. ref-1's desktop is in use, so the checks ran
    offscreen;
  - a build that finishes during Play (E3).

## As built (E2-1, 2026-10-20)

The transform gizmo (`docs/research/ux/viewport-tools.md` §2, §3), in
`engine_editor::gizmo`. It is drawn by the editor's overlay below its panels (layer −1),
clipped to the viewport.

- **Handles by tool** (Q, W, E, R, Godot's keys):
  - move: arrows, the planes' squares, a dot for the view's plane;
  - rotate: rings, the half facing the camera;
  - scale: lines with boxes, a box in the middle for all three;
  - select: all of them, the scale boxes halfway along the arrows.
- **Size and visibility:** 110 logical pixels at any distance. Handles seen edge-on are
  left out (a square thinner than 30 px², a ring within 9° of its edge, an arrow shorter
  than a quarter of the gizmo).
- **The pivot** is the middle of the selected entities that no selected entity carries.
  The axes are the world's, or the active entity's (Space).
- **Under the pointer,** in screen space: boxes and squares first, then the nearest line
  within 7 pixels.
- **A drag** meets the plane the handle moves in:
  - an axis's plane faces the camera most; a square's, ring's or view handle's is its
    own;
  - the change is snapped by the owner's rules through `Snapping::snap` (the inspector's
    `drag` now goes through it as well): a move's pivot coordinate along each axis, a
    turn's angle, a scale's factor.
- **Each frame of a drag** sets every target from where it started, in one undo step for
  the gesture (the inspector's merge key). Escape reverts it.
  - A move changes the local position only. A turn keeps the scale exactly; a scale keeps
    the rotation exactly.
  - Targets under a parent go through the parent's inverse world transform. A scale goes
    along the target's own axis nearest each handle.
- **Measured** on ref-1, release, 1600 × 900, 200 entities:
  - the editor's frame took 82.1 µs with nothing selected, and 91.4 µs with one selected
    and its gizmo (the inspector included);
  - the gizmo's layout and hit test took 3.5 µs for 101 lines.
- **Tested:**
  - projection and rays agree;
  - handles are found where they are drawn, and those seen edge-on are left out;
  - an arrow moves along its axis with Shift and Ctrl;
  - a ring turns about its axis in 45° steps;
  - a scale goes along the nearest own axis;
  - through the editor's frames: an arrow drag as one step and undone; Escape, and Ctrl
    on the grid; a turn about y keeping the scale; the tools' keys; a child of a turned
    parent moving along the world's x.
- **Not done:**
  - clicks and boxes in the viewport, and the selection's outline (E2-3 since: an id
    buffer, an ADR first);
  - keyboard transforms (G, R, S): R is the scale tool in Godot's keys, so the keys are
    the team's to choose (E2-2, below);
  - the snapping values in the tool bar; pivot modes; frame and bookmarks; surfaces;
    overlays.

## As built (E2-2, 2026-10-21)

Keyboard transforms and two key maps (team decision, 2026-10-20: Blender's keys by
default), in `engine_editor::transform` (`docs/research/ux/viewport-tools.md` §2, §8).

- **The key maps:**
  - Blender's, the default: G, R, S start a move, a turn, a scale; Q, W, E, T pick the
    gizmo's tools;
  - Godot's: Q, W, E, R pick the tools; G starts a move, and R or S during it switch to
    a turn or a scale;
  - "Editor: Key Map…" picks one, and the session keeps it (`keys = "godot"` in its
    `[editor]` block). The transforms are commands too ("Transform: Move Selection").
- **While one runs,** its keys, typed text and clicks are its own: the panels, the
  shortcuts and the camera get none of them (`InputCollector::consume`).
  - X, Y, Z keep it to an axis: the gizmo's axes first (the world's, or the active
    entity's after Space), then the other ones, then free. With Shift, to the plane
    across that axis.
  - These letters (and G, R, S while it runs) go by the virtual key, the letter on the
    key: Z on QWERTZ too, the Latin letter in Cyrillic layouts. Shortcuts stay physical
    keys.
  - A typed value (digits and `. , + - * / ( ) ^`, through `expr::evaluate`) takes the
    pointer's place. Text that does not evaluate yet (`2*`) keeps the last value;
    Backspace edits it. Without an axis, a move goes along x, a turn about the view's
    axis, and a scale is uniform.
  - G, R, S switch the transform: the targets go back, and the new one starts from the
    pointer as its own undo step.
  - Enter or a left click keeps it. Escape, a right click or the window losing the
    keyboard reverts it (the history's `revert`).
- **The pointer:**
  - a move meets a plane through the pivot: the view's, the one holding the axis that
    faces the camera most, or the plane kept to;
  - a turn adds up the pointer's angle about the pivot on the screen, frame by frame, so
    it goes past half a turn; its sign follows which way the axis faces;
  - a scale is the pointer's distance from the pivot over its distance at the start;
  - the owner's snapping rules hold as for the gizmo; typed values are exact.
- **Exact parts in the file,** for the gizmo too from this phase:
  - a turn sets the rotation from the target's own, the turn seen through the parent's
    inverse rotation;
  - a scale multiplies the target's own scale by its factors along its own axes;
  - so `R Z 90` and `S 0.5` leave no float dust from a matrix's round trip;
  - the active entity's axes lose components below 1e-6.
- **On screen:**
  - the gizmo hides;
  - the axis kept to (or the plane's two) runs across the viewport through the pivot, in
    its color;
  - a turn or a scale draws a dashed line from the pivot to the pointer;
  - a hint by the pointer says what it does ("Move along Z (local): 2 m", "Rotate about
    Y: 45°  ·  in steps"), and the status bar lists the keys.
- **Measured** on ref-1, release, 1600 × 900, 200 entities: the editor's frame took
  80.3 µs with nothing selected, 89.9 µs with one selected and its gizmo, and 103.5 µs
  during a keyboard move, the pointer moving every frame and the undo step set each
  time.
- **Tested:**
  - the module:
    - the axis keys cycle (the gizmo's axes, the others, free);
    - typed values and expressions: text that does not evaluate keeps the last value,
      letters are left out, a comma is a point;
    - the pointer along an axis, and free with Ctrl on the grid;
    - a turn past 270°, and in 45° steps;
    - a scale by distance and in a plane;
    - the guides across the viewport;
  - through the editor's frames:
    - G Z 25, Backspace, Enter as one step, then undone;
    - R Z 90 and S Shift+Z 0,5 with exact parts;
    - the pointer along x, then Escape, a right click (without a camera look) and the
      focus loss each taking it back;
    - a left click keeping it without grabbing the gizmo;
    - Godot's keys: R the scale tool, S free, G then R switching to a turn;
    - the key map picked in the palette and kept by the session.
- **Not done:**
  - Tab between a move's parts (Blender types x, y and z in turn);
  - typed constants (`pi`) and units, the letters being the keys';
  - the last operation adjustable afterwards, and Shift+R to repeat it;
  - navigating the camera during a transform;
  - picking and outlines (E2-3, an ADR first) and the rest of E2.

## As built (E2-2b, 2026-10-21)

Our key map and the camera's flight (team decision, 2026-10-21: Q, E, R transform at once,
W, A, S, D fly the camera as in Godot, the wheel sets the speed).

- **Three key maps,** ours the default:
  - ours: Q, E, R start a move, a scale, a turn; Shift+Q, E, R pick the gizmo's handles,
    and the active one's key again goes back to all of them;
  - Blender's: G, R, S; the tools on Q, W, E, T;
  - Godot's: the tools on Q, W, E, R; G, then R or S switch it;
  - "Editor: Key Map…" picks one and the session keeps it (`keys = "ofelia"`). While a
    transform runs, its map's letters (by the virtual key) start another over.
- **Flight** (`docs/research/ux/viewport-navigation.md`):
  - with the right button held, W, A, S, D move along and across the view, Q and E down
    and up the world's axis, and Shift moves three times faster;
  - its keys go by their physical place: the input collector keeps the held keys, which
    it did not before, and drops them when the window loses the keyboard;
  - each frame moves by the time since the last, at most a tenth of a second, so a stall
    does not throw the camera;
  - while flying no shortcut runs, so Q and E do not start transforms. The wheel sets the
    speed (5 m/s at first, ×1.2 a notch, within 0.05–1,000 m/s), which the session
    keeps (`fly = 5`). Without the right button the wheel zooms, as before;
  - the status bar says how to fly while it lasts.
- **Fixed:** a keyboard transform (and a gizmo drag) now counts as a gesture, so the
  autosave never writes a transform half done.
- **Tested:**
  - the camera's flight along the view, up the world's axis and level across;
  - held keys across frames and dropped with the focus;
  - through the editor's frames:
    - our keys: E, R, Q starting and switching transforms; G and S free; Shift+Q and
      Shift+Q again;
    - flying: W ahead and E up with Shift, each by its speed over a tenth of a second;
      the letters not running shortcuts; the wheel setting the speed, then zooming once
      the button is let go;
    - the key map and the speed kept by the session.
  - The earlier tests use our keys now (Q, E for moves and scales; Shift+R and Shift+E
    for the tools).
- **Not done:** acceleration while a key is held; flying with the middle button; focusing
  the selection (F).

## As built (E2-4, 2026-10-21)

- **F frames the selection** (with its children; nothing selected, the scene):
  - the command takes the entities' places; the overlay unites them with their world
    boxes from the GPU scene's frame (`SceneFrame::slot_box`, now public), or takes the
    scene's bounds;
  - the camera flies there in 0.2 s, easing out (`OrbitCamera::toward`: the target along
    a line, the turn the short way round, the distance in proportion), keeping its turn
    and elevation, from as far as fits the bounds' sphere in the view's narrower side
    with a tenth to spare (`OrbitCamera::framing`); a point gets half a meter;
  - a drag or the wheel on the camera takes it back from the move.
- **Shift+F follows** the active entity: its motion carries the target (and a move's
  end); a pan, a look around or a flight ends it, as does Shift+F again.
- **Bookmarks:** Ctrl+1..9 keeps the camera's place, 1..9 flies there (Unreal's keys, in
  every key map; digits run no shortcut while a field is typed in or a keyboard
  transform runs). The session keeps them in the scene's view (`bookmark = (n, x, y, z,
  yaw, pitch, distance)`).
- **Tested:** the framing's fit and the move's short way round; F asking for the
  selection's places or the scene, then flying to the overlay's bounds; following and
  stopping; bookmarks set, gone to and kept by the session; on the player, both GPUs: F
  puts the cube in the view's middle, three times larger on the screen.

## As built (E2-5, 2026-10-21)

- **Ctrl+Shift with a move handle** (an arrow, a square, the middle): each frame the
  overlay picks the surface under the pointer, the selection's instances left out
  (ADR 0109's surface pass); the newest one found places the selection, its world box at
  the press (the overlay keeps the selection's box every frame) touching the surface from
  the normal's side (`GizmoDrag::on_surface`): on a floor its bottom, on a wall its side.
  It runs a few frames behind the pointer.
- **End drops** each selected entity no selected entity carries, one after the other as
  one undo step: after a frame the entity's instances (with its children's) are named,
  the overlay unites their world boxes (its place for an entity without one) and picks
  the surface below the box's bottom's middle looking down, the selection and the entity
  left out; the entity moves straight down by the distance to it. "Nothing below" leaves
  it.
- **Tested:** the box on a floor, under a ceiling and against a wall; End's steps, one
  undo step, nothing below; Ctrl+Shift placing on a floor, then against a wall; on the
  player, both GPUs: End puts a cube floating 3.5 m above a floor on it (its center at
  0.5 above it, straight down), and Ctrl+Z puts it back.
- **Not done:** aligning to the surface's normal while dragging (the UX note's option);
  assets dropped from the file browser onto surfaces (with E4's browser); V's vertex
  snapping.

## As built (E2-6, 2026-10-21)

- **Steps in the tool bar** (the UX note's §3, Unreal's way): beside the grid button the
  position, angle and scale steps, each opening a menu of values (positions 0.01 to 5 m,
  angles 1° to 90°, scales 0.01 to 1; the current one checked), dim while snapping is
  off. Their labels are kept and made again only when a step changes (no text made each
  frame). "Snapping: Position Step…", "Angle Step…" and "Scale Step…" offer the same
  values in the palette; the session keeps them.
- **Pivots:** the middle of the selected entities no selected entity carries (the
  default), each entity's own origin (turns and scales about itself, the gizmo in their
  middle), or the active entity's origin (the gizmo there). The tool bar's right end and
  `.` (`Transform: Next Pivot`) cycle them; gizmo drags and keyboard transforms both
  follow (`Editor::set_targets`); the session keeps it.
- **Tested:** the steps' values in the palette, set and kept; a quarter turn of two
  entities about their middle, their own origins and the active one, and the pivot kept.
- **Not done:** typing a step not in the menus (the palette's list could take a number).

## As built (E2-7, 2026-10-21)

- **Icons** (the UX note's §7): the scene's lights and cameras (not the editor's own
  camera) show on the editor's canvas under the panels, clipped to the viewport: their
  kind's glyph (laid out by the UI's text cache, `UiText::draw_centered`) on a dark
  translucent disk at their place on the screen; the selected ones orange (the active one
  lighter), the one under the pointer white.
- **Picking icons:** they are above the scene, so the pointer within 12 px of one takes
  it before the GPU's picks: hover, a click (Shift and Ctrl toggle), and boxes (the icons
  inside a box join what it takes). A plain click again on the icon it selected goes
  through to the geometry under it (the cycling list), for a sun added where a cube is.
- **Reach:** the selected entities' as thin lines: a point light's range as three great
  circles, a spot light's cone (its base circle and four edges), a camera's pyramid (to
  2 m), the sun's direction (1.5 m).
- **"View: Overlays"** hides both; the session keeps it.
- **Tested:** icons for lights only, the editor's camera left out, at the orbit's target in
  the view's middle; hover, click, a click again going through, a box taking an icon;
  overlays off; each kind's reach lines; the session; on the player, both GPUs: a point
  light's, a camera's and the sun's icons drawn (a disk darker than around it, a light
  glyph), the selected spot light's cone (`ENGINE_EDITOR_OVERLAYS_PNG` writes the frame).
- **Not done:** sound sources' and reverb zones' gizmos (with E11), colliders and the
  navigation mesh (with physics and navigation in the editor); a viewport menu for
  overlays one by one.

## As built (the owner's notes after trying the editor, 2026-10-21)

- **The camera's drags lock the cursor** (the owner: hide the cursor, the mouse not moving):
  while the right or middle button drags the camera, the window thread hides the cursor,
  keeps it in the client area and puts it back where the drag began after every move,
  reporting the distance (`WindowEventKind::CursorDelta`, `WindowSystem::lock_cursor`,
  `Player::lock_cursor`), as Godot's captured mouse does; `WM_MOUSEMOVE`s coalesce, so
  no distance counts twice. It is let go on release and when the window loses the
  keyboard.
- **The eye in the tree** (the owner: "disabled hides and disables, as in Godot"):
  `engine_runtime::Disabled` (saved: `Disabled = {}`) turns an entity and everything below
  it off; `ActivityPropagation` keeps `Inactive` on each entity that is off, after every
  tick before the transforms, only when something was disabled, enabled or moved in the
  tree, marking the renderers and lights of entities that turn changed. Extraction draws
  an inactive chunk's meshes as hidden, the lights' extraction leaves inactive lights dark
  (their slots kept) and skips inactive suns, a blockout group leaves inactive brushes out.
  The tree's rows end in an eye (shut when disabled), dim when off; a click on it or **H**
  on the selection turns them off or on as one undo step. Physics and sound of what is off
  come with Play (E3): nothing simulates or plays while editing.
- **A game's development build** (the owner: "everything compiled in debug"): `ofeliaengine
  new`'s `dev` profile builds the engine as a release build (`opt-level = 3`, no overflow
  checks, 16 code units; before, its overflow checks and debug assertions stayed on, and
  `engine_rhi`'s debug assertions turned the Vulkan validation layer on), keeping a
  development build's sounds from the live folder, saves as text and allocation counters;
  `ENGINE_VALIDATION=1` (or `sync`) turns validation on. Playing is `cargo run` now, the
  same build as the editor's: the quick start's `cargo run --release` built the engine a
  second time.
- **Tested:** the cursor's distances drag the camera with the cursor still (the window's
  side is not tested: it would move the desktop's cursor); disabling a parent hides its
  child too and enabling brings both back with their changes since (extraction), lights
  dark and back in their slots, the sun off; H on the selection, undo and redo, the tree's
  eye and dim rows; on the player, both GPUs: a disabled group shows no brush and comes
  back, a disabled lamp saved and loaded off; no allocation per frame.

## As built (the owner's notes, the add menu, 2026-10-21)

- **The add menu groups its kinds** (the owner: meshes in a menu of their own, lights in
  theirs, brushes too, more groups later): Point, **Mesh** (cube, sphere, plane, cylinder),
  **Blockout** (the brushes, ADR 0110), **Light** (sun, point, spot), Camera; a group's
  kinds open beside it while the pointer is over it (`shell::MenuEntry`, the opened menu
  part of the popup for its dismissal). Every kind keeps its palette command ("Scene: Add
  Torus Brush").
- **"Empty" is "Point"** (the owner): an entity with a place and nothing else.
- **Snapping's key is G in our keys** (the owner: Y is an axis in a keyboard transform);
  Blender's keys Shift+Tab (Blender's own), Godot's Y (Godot's).

## Consequences

- **Games change shape:** data lives in scenes edited in the editor, logic in hooks and
  systems over components, and a game builds itself from its loaded scene (`start`). The
  quick start and `ofeliaengine new` moved to this shape in E1-5a.
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

## Addendum (E3-1, 2026-10-22): Play runs on the scene's world, put back at Stop

§2 had the play world as a copy of the edit world, with a runtime of its own on the GPU.
Building it showed what that copy costs.

**What is bound to one world.** The player keeps its state per world, beside the world
and keyed by its entities:
- the runtime's extraction and the GPU scene;
- `Assets`' and the blockout's queries and maps (groups and meshes by entity);
- the probes' and the GI volume's queries;
- the sound's sources (tracks by entity);
- the frame's camera lookups.

`engine_ecs` refuses a query on another world than its own (`world_id`), so a second world
would double each of these or rebuild them at every Play and Stop.

**The options:**

1. **A copy** (§2): the saves' capture into a second world, and a second set of everything
   above.
   - Two worlds can be on screen at once.
   - Every per-world piece of the player is doubled; the GPU scene and the models are made
     again at each Play.
2. **The scene's own world, kept and put back:**
   - Play records every scene entity's state as an undo step records it (components,
     with entity references by scene id, and the place in the tree).
   - It also keeps every entity alive with its components' types, the world's resources,
     the history's place and the selection.
   - Stop drops the game and ends what it left in the player (videos, voices, the network
     session: `Player::end_run`). Then:
     - entities made since go, and components of types an entity did not have are taken
       off (the game's own, runtime state);
     - the resources the game added go;
     - the scene's entities get their recorded state again, the despawned ones coming back;
     - the steps made while playing are forgotten (undo stops at the last step before Play
       while playing), and the selection comes back by scene id.

**Measured** (ref-1, release, `measure_play_snapshot_and_restore`):

| Phase | Time |
|---|---|
| Play: 10,000 scene entities recorded (a transform, a name, a tag with a string), 1.7 MB | 7.3 ms |
| Stop: put back, half of them moved and 1,000 spawned | 8.4 ms |

The budget is ≤ 1 s. The editor's test scene (five entities, no session to save) enters in
0.1 ms, its game made.

**Decision: 2.** What the developer sees is the UX note's (`play-mode.md`):
- one tree and one inspector on the running world, edits applying at once;
- the scene back at Stop.

Two worlds on screen (co-op in one process) will need worlds of their own and come with it.

**As built:**
- **Play and Stop:** F5 and F8 (Godot's keys; "Play: Play Scene", "Play: Stop") and the
  tab row's ▶ and ■.
- **Entering:** Play saves the session first, ends the tools and gestures, takes the
  editor's camera away (the scene's cameras show the game), keeps the world, and makes the
  game with its `start` on it.
- **While playing:**
  - The viewport shows the game without the editor's tools, outlines or icons, inside the
    `playing` edge.
  - Keys without Ctrl or Alt are the game's, but F1 to F12.
  - Saving, restarting and baking wait for Stop. So do the autosave and a build's restart.
- **A panic in a game's hook** is caught (`catch_unwind`, the hooks forwarded through
  `EditorGame`). Play stops, and the status bar says the message and the line (a panic
  hook of the editor's notes the place before the crash reporter's runs; that one still
  writes its report).
- **The world stands while editing:** `Player::set_simulating(false)`.
  - The ticks only place and extract; timers, tweens, the game's ticks and the sources'
    sound wait.
  - Before this, a scene's tween or sound changed the edit world without an undo step,
    and a one-shot sound could despawn its entity.
  - The same switch will be the pause.
- **Left for E3's next steps:** pause and step, play from the editor's camera, restart,
  keeping changes, the game's own window.

## As built (E3-2, 2026-10-22): pause, step, play from here, restart, eject

- **Pause (F7, ⏸ lit):**
  - `Player::set_simulating(false)` on the running game;
  - `EditorGame` calls none of the game's frame hooks (`event`, `net_input`, `present`,
    `animate`; `ui` still draws), so the frame stays where it stopped.
- **Step (Shift+F7):**
  - `Player::step_tick` runs the next tick as if the world ran (timers, tweens, the
    game's tick), once;
  - the game's frame hooks are called until it has run.

  F9 to F11 stay free for the script debugger's stepping (E9).
- **Play from Here (Shift+F5):**
  - the scene's main camera (the active scene camera of the highest priority) is put at
    the editor's camera, in its parent's space;
  - `engine_app::PlayFrom` (position and rotation) is in the world for the game's `start`;
  - both are after the world was kept, so Stop puts them back. The engine has no player
    pawn to place; the game places its own.
- **Restart (Ctrl+F5):** Stop and Play in the same frame, from here again if the last
  Play was.
- **Eject (Shift+F8, Unreal's F8 beside our Stop):**
  - the editor's camera, tools, outlines and keys come back on the running world, whose
    ticks go on;
  - the game's `engine_input::Input` listens to no device meanwhile (its filter kept and
    given back on returning or at Stop).

## As built (E3-3, 2026-10-22): keeping changes

- **What changed since Play** (`History::changes_since`): the scene's entities whose
  saved components (reflected, not `hidden` or `skip`) or place differ, by component;
  the entities made since; the scene's entities despawned since. Computed when asked,
  from the snapshot Play took. ECS change ticks are per chunk, not per entity, so the
  tree's per-frame marks on everything changed are left for later.
- **Marking:**
  - Ctrl+K keeps the selection's changes (Unreal's K, with Ctrl: plain letters are the
    game's while it plays); the tree's badge says "Kept";
  - Ctrl+Shift+K lists the changes in the palette, each line kept or not, the first
    keeping every change.
- **At Stop:**
  1. the kept entities' saved components and places are taken (`History::capture_kept`;
     entities made while playing get new scene ids);
  2. the world is put back;
  3. the kept states go in as one undo step, "Keep Changes" (`History::keep`): saved
     components set or removed to match, runtime components left alone, the missing
     ones spawned, kept despawns deleted.

## As built (E3-4, 2026-10-22): the game's own window

- **The options, as the rendering sees them:**
  - **A:** the scene drawn once, into the game's window, the editor's viewport saying where
    it is; a frame costs what it costs embedded.
  - **B:** both views every frame, the game's window and the editor's camera in the
    viewport; twice the scene's GPU time while playing.

  **Chosen: A.** Eject brings the view into the viewport (the game's window keeping its
  last frame). B can come later as a setting.
- **The player** (`Player::open_view_window`, `show_view_in_window`,
  `close_view_window`):
  - a second window, surface and swapchain (offscreen: an image of its own, read back
    by `capture_view_window`); the scene and the game's UI drawn into its image at its
    size; the main output cleared for the overlays;
  - one submission waits for both images and signals both presents
    (`Frames::submit_presenting`), and both are presented;
  - the view window's size, focus and close button kept apart from the main window's;
    the game's input released while it does not have the keyboard;
  - refused beside frame generation or an HDR10 output (the view stays in the main
    output).
- **NVIDIA (617.42):**
  - a submission drawing into a second swapchain crashed the driver
    (an access violation in `vkQueueSubmit2`) while the main swapchain was in Reflex's
    low-latency mode;
  - so did the frame after turning Reflex back on.

  Opening a view window therefore turns latency control off for the player's life, the
  main swapchain made again from nothing without the mode
  (`Swapchain::recreate_with_low_latency`: as a successor it kept the mode). The editor is
  not where latency is measured; the game's next start has it again.
- **The editor:**
  - Play opens the window as large as the viewport, titled "<scene> (playing)";
  - the viewport shows a word on where the game is;
  - F7 and F8 work in the game's window, its close button stops;
  - the game gets the events of the window it plays in;
  - "Play: In a Window or in the Viewport" chooses (in a window by default).

## As built (2026-10-22, the owner's notes): the mouse while playing, the game's window

- **The mouse** (`docs/research/ux/play-mode.md`'s "The mouse while playing"; the owner:
  the cursor hidden by default):
  - the game has the mouse from Play on: the cursor hidden and held over the window it
    plays in (`Player::lock_cursor` over the editor's, `Player::lock_view_cursor` over
    its own), while it runs (not paused or ejected);
  - "Play: Free the Mouse or Give It to the Game" (Shift+F1, in the game's window too),
    and a click in the game gives it back; the window losing the keyboard frees it;
  - while it has the mouse in the viewport, the panels get none of it (a button let go
    still, so none stays held); the status bar says how to free it;
  - a lock asked for while the cursor is off the window holds it in the window's middle
    (a game's window just opened, the cursor over the editor's).
- **The game's window:** as large as the viewport and at least 1280 × 720 (it was at
  least 640 × 360), fitted to its monitor; where it was when Play last ended, kept in the
  session (`game_window`).

## As built (E5-5a, 2026-10-22): scenes in tabs

- **Found:** the editor held one scene and could not open another; the session file
  already listed documents, each with its view.
- **An inactive tab is its scene's text** (`Doc`: the text as the tab was left, its
  file's base, autosave and view, its document in the history), not a world of its own:
  - switching saves the scene shown (instances as their overrides), lets go of what the
    editor held of it (gestures, the selection, picks under way, prefab bases), despawns
    its entities and loads the other's text, its view applied;
  - an inactive scene follows a prefab written anew by itself, its text holding only the
    overrides; one with nothing unsaved whose file changed on disk is read again;
  - measured (ref-1, release, 10,000 entities with a name, a transform and a model):
    shown again 18.9 ms, left for a small scene 10.7 ms. A world per tab (every system
    that holds queries or entities of the player's world following a swap) was not
    needed for that.
- **One history across tabs** (§4): each step names its document
  (`History::set_document`); a scene's unsaved state compares its own head
  (`History::head_of`), so steps in other tabs leave it as it is; undo and redo of
  another tab's step show that tab first (`undo_document`, `redo_document`); a closed
  tab's steps go (`forget`); a document whose steps the budget dropped keeps its head.
- **Commands** (Godot's keys): File: Open Scene… (Ctrl+O, the palette's scenes; a double
  click in the dock), New Scene (Ctrl+N), Close Scene (Ctrl+W, a tab's ×), Reopen Closed
  Scene (Ctrl+Shift+T), Next and Previous Scene Tab (Ctrl+Tab, Ctrl+Shift+Tab).
- **Closing** asks nothing (`docs/research/ux/sessions.md` §8): unsaved changes stay in
  the autosave written when the tab was left; the last 20 closed scenes can be opened
  again with them (opening the file again does too), marked when the file changed on
  disk since. The last tab stays open.
- **The session** lists every tab in its order with its base, autosave and view, and the
  one shown (`active`); a start opens them all, the inactive ones from their autosaves
  (unsaved, marked when their files changed since) or their files, and shows that one.
- **Not while playing:** switching, opening and closing wait for Stop.
- **Left:** E5-5b, a prefab opened in a tab from its instance; tabs dragged into
  another order; a scene open in two tabs.

## As built (E4-1, 2026-10-22): the local cache's index

§5 left the index's form to a measurement: our own text file, read in 3.2 ms and an
unchanged project of 10,000 files scanned in 20 ms (ADR 0108's as-built of the same day),
so not SQLite. The editor opens it at its start and follows the folder.

## As built (E4-2, 2026-10-22): the asset browser

- **The FileSystem dock** (the layout's, under the viewport) is filled from the asset
  database:
  - the folders that hold assets in tree order, `res://` at the top;
  - the open folder's subfolders, then its files with their kinds' icons, the picked
    file lit.

  The lists are made again when a scan or a click changes them, not every frame.
- **`#` in the palette** finds assets by path through the palette's matcher. A kind
  before a colon narrows it (`#model:crate`; `mesh` and `audio` too), and Enter opens the
  asset's folder with it picked.
- **Left for E4's next steps:** drag and drop from the dock (models into the viewport and
  the tree, materials onto objects), thumbnails, import settings in the inspector,
  renames inside the editor, references.

## As built (E4-3, 2026-10-22): drag and drop from the dock

- **Starting a drag:** a file of the listing held and moved 4 px. The hint by the pointer
  says what letting go does, the tree row it would land on is outlined, and Escape ends
  it. The keys are the drag's while it lasts.
- **A model into the viewport:**
  - **The plan:** its entities are planned from the file's outline (`GltfOutline`:
    nodes, meshes, primitives' materials and bounds, from the JSON alone; a `.glb`'s head
    and JSON chunk only).
  - **The preview:** the entities are placed while the pointer is over the view, in one
    undo step, and selected, so the surface pick under the pointer leaves them out.
  - **The place:** the model's box rests on the surface found, by the normal. With no
    surface, it rests on the plane y = 0 within 200 m, else 10 m ahead. With snapping,
    it snaps along the surface.
  - Leaving the view or Escape reverts the step and puts the selection back.
- **Other drops:**
  - a model onto a tree row goes under that entity, at its origin, and onto the tree
    below the rows it becomes a root where the camera looks;
  - a material file onto an object (the hover pick) or a row is given to its model;
  - any asset onto an inspector asset field is set through the fields' path, on every
    selected entity, with `Model.mesh` and `Model.material` checked for what fits.
- **Names:** parts are named by their names when unique, else `#i`. A file of one node is
  one entity named after the file. Unnamed nodes take their mesh's name.
- **Measured:** nothing; reading an outline is one small file read when the drag first
  needs it.
- **Left:**
  - meshes of several materials draw with their first: per-primitive parts or material
    slots on `Model` is a format question, put to the team;
  - live material previews while hovering;
  - aligning to the surface's normal;
  - scenes dropped as instances (prefabs).

## As built (E4-7, 2026-10-22): renames, moves and references

- **Renames:** F2, after a click in the listing (the dock's focus, as the tree's), turns
  the picked tile's name into a text field. Enter renames the file in its folder, its
  extension kept when left out; names with path characters are refused.
- **Moves:** a file dragged onto a folder tile or a row of the folder tree moves into it
  (the tiles and rows are hover regions under a drag).
- **What follows a rename or a move** (`move_asset`):
  - the file's `.meta` goes with it, so the database finds the same id at the new path at
    once, without waiting for the watcher;
  - the paths of its parts (`file#part`) are rewritten in the editor's and the player's
    asset paths;
  - an open scene renamed keeps being the open scene;
  - scenes keep their references, which are ids, and a scene saved after it writes the
    new path.

  Neither a rename nor a move is an undo step: they change files on disk, as Godot's
  and Unity's do.
- **Find References** (Shift+F12, "Assets: Find References") lists where the dock's
  picked asset is used, in the palette:
  - the open scene's entities whose saved lines name its id, as the scene is now with
    each line's field (Enter selects one);
  - the project's scenes and materials on disk that name it, with how many times (Enter
    shows the file in the dock).
- **Left:** renaming folders; "Dependencies" (what an asset uses); references in
  prefabs, which come with E5.
