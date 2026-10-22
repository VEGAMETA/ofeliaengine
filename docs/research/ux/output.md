# Output, causes, tasks and history (E6)

Checked: 2026-10-22. Products were studied as products, from their documentation; facts
marked * were not found there again today (from use and older documentation). Godot (MIT)
was read for ideas only. No code was carried over. Input for ADR 0107's E6.

CLAUDE.md §12: errors next to their cause, a click taking you to the object, asset or
line; nothing blocks the UI, background tasks in a task panel; every action undoable, with
a history panel.

## How five editors do it

| Editor | The log | From an error to its cause | Background work | History |
|---|---|---|---|---|
| Unity ([Console](https://docs.unity.com/en-us/engine/6000.3/manual/unity-editor/editor-windows-views-reference/console-window/console.md)) | the Console: filters by level, search; Collapse shows a repeated message once; Clear on Play, on Build, on Recompile; Error Pause pauses Play on `Debug.LogError` | stack traces link to their lines*; a log given a context object highlights it in the hierarchy* | a progress bar in the status bar and a Background Tasks window* | Edit > Undo History* |
| Godot 4 ([Output panel](https://docs.godotengine.org/en/4.4/tutorials/scripting/debug/output_panel.html)) | the Output panel: Log, Error, Warning and Editor toggles, a text filter; "Always Clear Output on Play" | the running game's `push_error` and `push_warning` go to the Debugger's Errors tab, not the Output | modal progress dialogs while importing* | the History dock: the scene's and the global history* |
| Unreal | the Output Log: categories and verbosity (Error to VeryVerbose), filters by category and text | the Message Log's entries link to actors and assets* | notifications with progress and cancel* | Edit > Undo History* |
| VS Code | Output: a channel per tool | Problems: errors grouped by file, a filter, a click opens the line | progress in the status bar and notifications | — (undo per document) |
| Blender | the Info editor: reports and the operators run, as Python | — | a progress bar in the status bar | Edit > Undo History: the steps, a click goes back |

**What annoys:**
- Unity's per-frame errors flood the Console without Collapse.
- Godot splits one game's messages between two panels: Output, and the Debugger's Errors
  tab.
- Unreal's Message Log is a second place to look, beside its Output Log.
- Godot's import dialogs block the editor.

## Scenarios

| The developer wants to… | Ours |
|---|---|
| see why the game misbehaves while playing | Output (a tab of the bottom panel) shows the log as it comes, the newest last, errors marked; a text filter and toggles for each level |
| not drown in a per-frame error | the same message again folds into its row, with a count (Unity's Collapse), on by default |
| tell this run's messages from the last | a line marks where each Play started; "Clear" empties it |
| go from an error to its cause | a project asset's path in a message shows it in the FileSystem dock; a scene entity's id selects it; `file:line` opens the line (VS Code now, the code editor with E8) |
| see what runs in the background | Tasks: files loading, textures compressing, thumbnails, the navigation bake, the game's build, each with how far it is |
| go back several steps at once | History (beside the scene tree): the steps named, the current one marked, other tabs' steps with their scene's name; a click undoes or redoes to it |

## What we take

1. **One bottom panel with tabs** (`editor-layout.md`): Output, then the others as they
   come. It opens and closes from its tab bar, and with Ctrl+J, VS Code's key.
2. **The log is the engine's** (`engine_log`, ADR 0061): every record in the ring, with its
   time, level and target. The panel reads the records added since its last frame, without
   locks or allocation in the logger. It keeps its own rows, the last 10,000.
3. **Repeats fold** (Unity's Collapse), on by default: a count on the row, its time the
   last one's.
4. **One place for a run's messages:** the game's and the engine's, unlike Godot's two.
5. **Causes from the message's text:** project asset paths (matched against the asset
   database), scene entities' ids, `file:line`. Nothing in the logging calls changes, and
   the engine's messages already name their files.
6. **Tasks and History next** (E6's further steps), the profiler's first panel after them.

## What we do not take

- Modal progress dialogs (Godot's imports).
- A second log for some subsystems (Unreal's Message Log).
- Clearing on Play by default: the line that marks it keeps the last run readable.
