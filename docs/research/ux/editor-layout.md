# Editor layout and workflow: Godot as the base, better where it hurts, less of the rest

Checked: 2026-09-26. Godot 4.x (MIT) was studied as a product and, where useful, its
code was read for ideas; no code was carried over. Unity, Unreal, Blender and VS Code
were studied as products.

The team works in Godot and likes it. The goal: Godot users feel at home on day one,
things that slow them down in Godot are fixed, and features that add weight without
value are not built (CLAUDE.md §12: design from scenarios, remove the excess).

## What we keep from Godot (familiar on day one)

- **Default layout** (the team's own Godot setup, 2026-09-26):

  ```text
  ┌ scene tabs ───────────────────────────────┬──────────────┬──────────────┐
  │ tool bar (select/move/rotate/scale…)      │ Scene        │ Inspector    │
  ├───────────────────────────────────────────┤ (+ History,  │ (+ Signals)  │
  │                                           │  Groups…)    │              │
  │               viewport                    │ filter       │ filter       │
  │                                           │ node tree    │ properties   │
  ├───────────────────────────────────────────┤              │              │
  │ file browser: folder tree │ file grid     │              │              │
  ├───────────────────────────────────────────┴──────────────┴──────────────┤
  │ Output · Debugger · Audio · Animation · Shader · Files       status/ver │
  └──────────────────────────────────────────────────────────────────────────┘
  ```

  - The scene tree and the inspector sit side by side at full height on the right.
    Both are tall lists, so neither is squeezed.
  - The file browser sits under the viewport.
  - Bottom panels are tabs in a thin bar and open on demand.
  - Every dock can be moved, and layouts are saved.
- **Rendering scale for weak hardware** (Godot's 3D scaling modes): render scale with
  bilinear or nearest upscaling (viewports), later FSR 1 and the temporal upscalers.
- **Play, pause and stop are quiet:**
  - small monochrome icons at the right end of the tab row, not a coloured
    centre-stage group;
  - F5 / F6 / F8 as in Godot;
  - while playing, a thin coloured edge around the viewport and a status-bar note show
    the state, instead of big buttons.
- **Scene tree → inspector:** select in the tree or the viewport, edit on the right;
  drag to reparent; right-click for add and duplicate.
- **Quick open** of files and scenes by name.
- **A Godot keymap preset** (`Keymap::from_text`, ADR — command palette PR): the same
  shortcuts where the actions exist, so muscle memory works. Presets for Blender and
  Unity can follow.

## What we do better than Godot

| Godot friction | What we do instead |
|---|---|
| Separate 2D / 3D / Script / AssetLib workspaces (a mode switch at the top) | No workspaces: viewports are panels (2D and 3D side by side if wanted); scripts open in VS Code with luau-lsp and the debugger (DAP). No modes (Raskin). |
| The game runs in a separate process; the remote tree is read-only-ish; changes made while playing are lost | **The game runs in its own window by default** (the team tests that way), but in the same process as the editor, so starting is fast and everything stays live: select, inspect and edit objects of the running game from the editor. Changes made in Play are highlighted, and **"Keep changes"** applies them to the scene with undo. Options: embedded in the viewport, or a separate process (crash isolation, closest to a shipped build). |
| Undo history per scene, sometimes surprising | One global undo with a history panel (ADR 0031); every step named. |
| Finding a property in a long inspector | The inspector has an always-visible filter. It can show only values that differ from the default or the prefab. Every field has reset-to-default and "overridden by" markers. |
| Number fields | Drag to scrub (Shift for fine, Ctrl for snapped); expressions (`2*pi`, `+=10`); units shown. |
| Transform tools as mode buttons | Keyboard gizmos: G/R/S with axis keys and typed values (Blender). The Godot W/E/R stay in the Godot preset. |
| Project Settings and Editor Settings as big dialogs | Every setting is found through the command palette and one search box. No modal dialogs (§12). |
| Manual reimport, import dock | Background import on file change; errors appear on the asset with a click to the cause. |
| Signals connected through a dialog | Scripts connect in code with typed events (generated `.d.luau`); the inspector shows existing connections read-only. |
| Instanced/inherited scenes are hard to reason about | Prefabs (post-v1 of the scene format) show overrides inline: each field says whether it comes from the prefab or the instance. |

Across all of it:
- **One search box** (the command palette with prefixes): commands, scene objects,
  assets, settings.
- **Errors next to their cause:** clicking one jumps to the object, asset or line.
- **Drag an asset into the viewport:** it lands on the surface under the cursor
  (raycast), aligned to the normal.
- **Frame selected (F), camera bookmarks (Ctrl+1..9), snapping toggles on keys.**

## What we cut (not building)

| Cut | Why |
|---|---|
| Asset Library tab | A browser inside the editor adds weight for little use; assets come from files and version control. |
| A full built-in code editor | VS Code with luau-lsp, types and the DAP debugger is better than anything we would build. The editor keeps a quick-edit view for small changes and jump-to-line. |
| Visual scripting | Godot dropped it too; Luau plus good tooling covers it. |
| Theme editor | Theme tokens in code and a few presets (dark, light, high contrast) cover accessibility (§12). |
| Workspace/mode tabs | Replaced by panels (above). |
| Modal progress and confirmation dialogs | Background tasks with a task panel; destructive actions are undoable instead of confirmed. |
| Editor plugins in the first versions | Tools come from the engine team first; a plugin API follows once the core tools are stable. |

## Build order for the editor

1. Docking shell with the Godot default layout, command palette, global undo (ADR
   0030, 0031, palette PR).
2. Scene tree and inspector (reflection-driven, multi-edit, filter, overrides).
3. Viewport with keyboard gizmos, selection, frame, drag-and-drop from the file
   browser.
4. Play in a separate window (same process) with live editing and "Keep changes"; embedded and separate-process options.
5. File browser with background import and previews; output and error panels.
