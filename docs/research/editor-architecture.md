# The editor's architecture: research

Checked 2026-10-17. Input for [ADR 0107](../adr/0107-editor.md) and
[ADR 0108](../adr/0108-scene-format-2.md). Engines were studied as products through their
documentation; Godot (MIT) and Fyrox (MIT) were read for ideas. No code was carried over.

## Questions

1. Where the game's code runs relative to the editor.
2. How Play works: which world runs, in which window, what happens to changes made while
   playing.
3. How an edit of the game's code reaches the editor.
4. How scenes refer to assets, so that renames do not break them.
5. What survives a crash or a restart.

## Sources

| Source | Checked | License | What was looked at |
|---|---|---|---|
| Unreal Engine 5 documentation: Play In Editor, Simulate, "Keep Simulation Changes", Live Coding, redirectors | 2026-10-17 | proprietary (product) | PIE as a copy of the editor's world, hot patching of C++ modules |
| Unity 6 manual: Play mode, "Enter Play Mode Options", asset metadata (`.meta`) | 2026-10-17 | proprietary (product) | domain and scene reload, GUIDs in sidecar files |
| [Godot docs: Game embedding](https://docs.godotengine.org/en/4.7/tutorials/editor/game_embedding.html) and the `uid://` system (4.x) | 2026-10-17 | MIT | the game in a separate process, embedded or in a floating window, selection in the running game |
| [Fyrox](https://fyrox.rs) 0.34 release notes, `fyrox-template` | 2026-10-17 | MIT | the editor linked with the game as a plugin; hot reload through a dynamic library |
| Bevy 0.15+ (Bevy Remote Protocol), `bevy-inspector-egui` | 2026-10-17 | MIT / Apache-2.0 | no official editor; in-process inspectors and a remote protocol |
| [Defold manual: debugging game logic](https://defold.com/manuals/debugging-game-logic) | 2026-10-17 | Defold License (product) | the engine as a separate process, resources reloaded into it |
| [Subsecond](https://docs.rs/subsecond/) (Dioxus 0.7) and [bevy_hotpatching_experiments](https://docs.rs/crate/bevy_hotpatching_experiments/latest) | 2026-10-17 | MIT / Apache-2.0 | hot patching of Rust functions through a jump table |
| Our measurements: `docs/dev-guide/editor.md`, `docs/dev-guide/quick-start.md`, ADR 0104 | 2026-10-17 | — | cold start 0.39 s, a game's rebuild 2.5 s, a world copied in 0.023 ms (10,000 entities) |

## How others do it

### 1. Where the game's code runs

| Engine | Model | What it costs |
|---|---|---|
| **Unreal** | The editor is the engine built with the game's C++ modules (DLLs). | Native types everywhere. A crash in game code takes the editor down. Code edits: Live Coding patches functions in place; layout changes need a restart. |
| **Unity** | The editor is a fixed product; game code is C# assemblies it loads. | Reloading the assemblies (domain reload) takes seconds on big projects and wipes static state. |
| **Godot** | The editor is the engine binary; scripts are interpreted; C# and GDExtension are loaded. | The game runs in a **separate process** (F5): crash isolation, but the editor sees the game only through a remote protocol. |
| **Fyrox** (Rust) | `fyrox-template` makes three crates: the game, an editor that links the game statically as a plugin, and an executor. Since 0.34 the game can also be built as a dynamic library and reloaded into the running editor. | The static link is our option A. The dynamic library works because Fyrox controls both sides of the boundary, but Rust has no stable ABI: types must not change shape across a reload. |
| **Bevy** (Rust) | No official editor yet. Inspectors run in the game's process (`bevy-inspector-egui`); the Bevy Remote Protocol (JSON-RPC) lets tools outside the process read and write the world. | In-process inspection is easy for Rust; a remote protocol is what an out-of-process editor needs. |
| **Defold** | The editor (JVM) starts the engine as a separate process and pushes changed resources into it. | Isolation and hot reload of resources; the engine's state is only seen remotely. |

**For a Rust engine,** linking the editor with the game (Fyrox, Unreal) is the only model
where the game's own components and systems are native in the editor without a protocol
or an ABI contract. The owner chose it (ADR 0107, option A).

### 2. Play

- **Unreal, Play In Editor (PIE).**
  - It duplicates the editor's world in memory and runs the copy, in the editor's viewport
    or in a new window. The editor's world is untouched.
  - Simulate (Alt+S) runs the copy without a player.
  - F8 ejects from the player's pawn to fly freely.
  - **K, "Keep Simulation Changes,"** copies a selected actor's state back into the
    editor's world.
  - Standalone Game is a separate process.
  - Several clients with a server can run in one process (multiplayer PIE).
  - Entering takes seconds: nothing is loaded from disk.
- **Unity.**
  - Play runs the open scene itself after reloading scripts and the scene. When Play ends,
    the scene is restored, and **changes made while playing are lost**: a tint is the only
    reminder.
  - "Enter Play Mode Options" can skip both reloads and enter almost at once, at the price
    of static state left over from the last run.
- **Godot.**
  - F5 runs the project, F6 the current scene, in a separate process.
  - Since 4.4 the game can be embedded in the editor or float in its own window (the
    default). Its Game bar selects nodes in the running game and shows them in the
    inspector.
  - Edits made that way are temporary. To keep an edit, the user changes the node in the
    editor's own ("local") scene tree, and "Synchronize Scene Changes" pushes it into the
    running game.
- **Defold:** a separate process; changed resources are hot-reloaded into it.

What hurts:
- losing changes made while playing (Unity, Godot);
- two trees, the "local" one and the "remote" one, with different rules (Godot);
- slow entry (Unity by default).

What works: Unreal's copy of the world, which makes entry fast and leaves the editor's
world untouched, and its K, which keeps a change made while playing.

### 3. Code changes

- **Unreal Live Coding** recompiles the changed module and patches functions in the
  running editor. A change of a class's layout still needs a restart.
- **Unity:** a domain reload after every compile.
- **Fyrox:** the game as a dynamic library, reloaded with its state serialized and
  restored around the reload.
- **Subsecond** (Dioxus 0.7, MIT/Apache-2.0):
  - it hot-patches Rust functions: calls go through a jump table that points at the latest
    version of each function;
  - works on Windows, but is marked experimental;
  - follows only the "tip" crate (the game), not its dependencies, which matches a game
    on the engine;
  - Bevy has a community integration (`bevy_hotpatching_experiments`).
- **A restart with the session restored:** none of the engines above make it their main
  path, but every one of them restores a session after a crash (Unreal and Blender
  autosave, VS Code's "hot exit" brings back unsaved files without a prompt). With a
  0.39 s cold start, a restart that restores the session is cheap.

### 4. Asset references

| Engine | How a reference is written | Renames |
|---|---|---|
| **Unity** | A GUID (plus a local file id). The GUID lives in a `.meta` file next to every asset, with its import settings. | Safe inside the editor; outside it the `.meta` must move along. Diffs show GUIDs, not names. |
| **Unreal** | An object path (`/Game/Props/Crate.Crate`). | Renaming in the editor leaves a redirector that is fixed up later. |
| **Godot 4** | A `uid://…` and the path, both written (`[ext_resource … uid="uid://…" path="res://…"]`). The uid lives in the `.import` sidecar of imported files, inside text resources, and since 4.4 in `.uid` files next to scripts and shaders. | The uid finds a moved file; the path keeps diffs readable and is the fallback. |
| **Bevy** | Paths; processed assets may carry meta files with settings. | Renames break references. |

Godot's hybrid keeps both properties: a stable identity and a readable diff. Sidecar files
also hold import settings, which we already keep beside sources in one case
(`<scene>.colliders.ron`, ADR 0101).

### 5. Sessions and crashes

- **Autosave** to a separate place, not over the user's file (Unreal, Blender).
- **Recover the last session** after a crash (Blender's `quit.blend`, Unreal's restore
  dialog). We do it without a dialog, like VS Code's hot exit.
- **The layout,** open documents, cameras and selection come back after a restart
  (every editor above).

## What we take

- **The editor linked with the game** (Fyrox, Unreal): one process, the game's types
  native. The owner's option A; a separate-process mode comes later for crashes.
- **Play on a copy of the world** (Unreal's PIE):
  - the game in its own window by default (Godot's default), embedded as an option;
  - the editor's tree and inspector show the running copy, and edits apply at once;
  - **"Keep changes"** (Unreal's K) for a selection or everything changed, as one undo
    step.
- **Code changes:**
  - a build in the background while the editor keeps working, then a restart that brings
    the session back within about a second;
  - later, Subsecond-style hot patching of function bodies, evaluated as CLAUDE.md §10
    asks.
- **Asset references:** Godot 4's hybrid of an id and a path, ids and import settings in
  a sidecar per source file. It is a file format, so the owner decides (ADR 0108).
- **Sessions:** autosave beside the project, recovery without a dialog. One mechanism
  serves crashes and rebuild restarts.

## What we do not take

- **Unity's domain reload and lost Play changes.**
- **Godot's two trees** (local and remote) with different rules for keeping changes.
- **The game as a dynamic library** for now. Rust has no stable ABI, and the type list
  (`linkme`) and reflection tables would have to be rebuilt across reloads. Hot patching
  function bodies gives most of the gain with less risk.
