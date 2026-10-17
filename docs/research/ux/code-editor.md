# Code editor

Checked: 2026-10-17. Products were studied as products (behaviour, documentation); Godot
(MIT) and the Luau playground (MIT) were read for ideas only. No code was carried over.
Input for [ADR 0107](../../adr/0107-editor.md).

The owner, 2026-10-17: a built-in code editor at the level of Godot's or Roblox Studio's
(option Б), not a replacement for VS Code, which keeps working beside it. This replaces
the cut in `editor-layout.md` ("A full built-in code editor").

## How five tools do it

| Tool | What it has | What works | What annoys |
|---|---|---|---|
| Godot 4 (script editor) | completion, errors and warnings as you type, breakpoints in the gutter, Ctrl+click to a symbol, built-in docs, find in files, several carets | everything in one window; jump from an error to its line | a separate Script workspace: the scene is out of sight while coding |
| Roblox Studio (script editor) | Luau type checking (`--!strict`), completion with types, a list of all problems (Script Analysis), go to declaration, a debugger with breakpoints, watch and call stack | the reference for Luau specifically: types, completion and debugging built in | tied to Roblox's world |
| Defold (code editor) | completion and linting through Lua Language Server and Luacheck; breakpoints in the gutter with conditions; an integrated debugger (break, step, call stack with variables, an evaluation console) | a small engine editor that still has all of it | variables cannot be changed from the console |
| The Luau playground (play.luau.org, [repository](https://github.com/luau-lang/playground)) | CodeMirror 6 with a Luau grammar; Luau's own analysis compiled to WebAssembly: diagnostics, completion with types, hover; bytecode view | about 1,400 lines of C++ glue give every smart feature, because Luau's analysis library does the work | no debugger |
| VS Code (with luau-lsp) | the reference for editing: several cursors (Alt+click, Ctrl+D), F12 definition, Shift+F12 references, F2 rename, Ctrl+Shift+F, inline diagnostics, hover cards | most developers' hands know its keys | not inside the game's editor: errors, scene and game live in another window |

## Scenarios

| The developer wants to… | Ours |
|---|---|
| fix an error the game reported | click it in the output: the script opens at the line, the error underlined and its message at the end of the line; fix it; Ctrl+S reloads the script in the running game, state kept |
| give an entity new behaviour | "Add component", Script, "New script": `scripts/<name>.luau` from a template, opened with the cursor in the hook |
| call an engine function they half remember | type `transform.`: completion with types and the documentation from the generated `.d.luau` |
| change a name everywhere | F2 on it: renamed across the project's scripts (later than the rest) |
| do a long refactor in VS Code | "Open in VS Code" at the same line; edits made there are picked up by the file watcher |

## What we take

1. **Scripts open as document tabs** beside scenes (VS Code, Godot's script list) in the
   editor's central area. The game plays in its own window by default, so code and game
   sit side by side; the viewport can be split beside the code. No workspace switch.
2. **Editing keys are VS Code's** by default:
   - several cursors: Alt+click, Ctrl+D (next match), Ctrl+Alt+Up and Down;
   - Ctrl+/ comments, Alt+Up and Down move lines;
   - F12 definition, Shift+F12 references, F2 rename;
   - Ctrl+Space completion, Ctrl+F and Ctrl+H find and replace, Ctrl+Shift+F find in files,
     Ctrl+G go to line.
3. **Smart features from Luau's own analysis library**, linked into the editor as the
   playground does: diagnostics as you type, completion with types, hover, signature
   help, go to definition, references, later rename.
   - It runs off the UI thread.
   - It reads the same generated `.d.luau` that VS Code's luau-lsp reads, so both
     editors agree.
4. **Errors inline:** a squiggle, the message at the end of the line, and the list in the
   output, each a click from its line (§12).
5. **Save reloads the script** in the running game, state kept (ADR 0032, ≤ 100 ms). Code
   that does not compile keeps the old version running and shows the error at its line.
6. **The debugger lives in the gutter:** breakpoints, conditions, logpoints, the current
   line, values beside the code while paused (`script-debugger.md`).
7. **VS Code stays first-class:** "Open in VS Code" at the same line. A file changed
   outside is reloaded if it has no unsaved edits; with unsaved edits a banner offers to
   compare, never a dialog.
8. **Speed:** a key press reaches the screen in the same frame; highlighting is
   incremental per line; a 10,000-line file scrolls at the display's rate.

## What we do not take

- A separate Script workspace (Godot).
- An IDE: version control, extensions, terminals (VS Code does that).
- A language-server protocol between our own editor and the analysis library: it is
  called in process.
