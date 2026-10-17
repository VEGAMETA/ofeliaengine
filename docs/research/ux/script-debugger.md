# Script debugger

Checked: 2026-10-17. Products were studied as products (behaviour, documentation); Godot
(MIT) was read for ideas only. Luau's debugging interface was read in the sources we build
(`luau0-src` 0.21.0+luau736: `VM/include/lua.h`, `VM/src/ldebug.cpp`, `VM/src/ldo.cpp`).
No code was carried over. Input for [ADR 0107](../../adr/0107-editor.md).

CLAUDE.md §10: a DAP debugger (breakpoints, stepping, watch, call stack) for VS Code and
built into the editor; script errors do not crash the game in the editor. §12: the script
debugger is a required tool.

## How four tools do it

| Tool | What it has | What works | What annoys |
|---|---|---|---|
| VS Code (Debug view) | Variables by scope, Watch, Call Stack, Breakpoints; conditional breakpoints, hit counts, logpoints; values shown inline in the code while paused; a debug console | the layout and keys most developers know: F5 continue, F9 breakpoint, F10 over, F11 into, Shift+F11 out | outside the game's editor |
| Godot 4 (Debugger panel) | a stack with its variables, breakpoints in the script editor, break and continue, step into and over; errors, profilers and monitors in the same panel | built into the editor | the running game's state is seen through a separate remote tree |
| Roblox Studio | breakpoints in the gutter, watch, call stack; conditional breakpoints and logpoints | a Luau debugger built into an editor, the closest model | — |
| Defold | start or attach; break, continue, step over, into, out; breakpoints with conditions; the call stack with each frame's variables; an evaluation console | complete in a small editor | variables cannot be changed from the console |

## What Luau gives us (read in the sources we build)

- **Breakpoints:** `lua_breakpoint(L, funcindex, line, enabled)` puts a `LOP_BREAK` over
  the first instruction of the line (or the next line with code) in the function and the
  functions nested in it. The `debugbreak` callback runs when it is hit.
- **Stepping:** `lua_singlestep(L, enabled)` calls `debugstep` after every instruction;
  the interpreter switches to its stepping loop only while it is on.
- **The stack and variables:** `lua_getinfo`, `lua_stackdepth`, `lua_getlocal` and
  `lua_setlocal` (variables can be changed), `lua_getupvalue`.
- **Errors:** `debugprotectederror` runs when a protected call fails, at the failing
  point.
- **Pausing without freezing:** inside `debugbreak`, `lua_break(L)` suspends the script
  like a yield: `lua_resume` returns `LUA_BREAK`, the game's frame goes on without the
  script, and resuming continues at the breakpoint. It cannot cross a C call boundary
  (`attempt to break across metamethod/C-call boundary`); scripts must be entered as
  resumable threads for it.
- **Native code:** native code has no breakpoints. Setting one in a natively compiled
  function turns that function back to the interpreter (`ecb.disable`), and it stays
  interpreted until it is reloaded. Without breakpoints nothing changes.
- `mlua`'s raw bindings (`mlua::ffi`) expose all of it.

## Scenarios

| The developer wants to… | Ours |
|---|---|
| stop where health goes negative | click the gutter: a breakpoint; right-click, condition `hp < 0`; play; it stops there with values beside the code |
| log without stopping | a logpoint: `hp = {hp}` goes to the output when the line runs |
| see why a script failed | "pause on errors" (on by default in the editor): the failing line, its stack and variables |
| look at the world while a script is stopped | the game's ticks stop, the editor keeps working: select entities, read and change components, then continue |
| fix a value and go on | change a local in the Variables panel (`lua_setlocal`), F5 |
| debug from VS Code | attach VS Code to the running editor or game (DAP over localhost): the same breakpoints |

## What we take

1. **VS Code's panel and keys:**
   - the panels: Variables (locals, upvalues, globals), Watch, Call Stack, Breakpoints,
     and a console that evaluates in the paused frame;
   - the keys: F5 continue (and play), F9 breakpoint, F10 over, F11 into, Shift+F11 out.
2. **Breakpoints:** conditions, hit counts, logpoints (VS Code, Roblox, Defold).
   Breakpoints survive reloads: they are re-applied to the new functions.
3. **Values beside the code** while paused, and on hover (VS Code).
4. **Variables can be changed** (unlike Defold): `lua_setlocal`, upvalues, table fields.
5. **Pause on errors** by default in the editor (CLAUDE.md §10: show the error, pause the
   object, jump to the line).
6. **The game pauses, the editor does not:** a breakpoint suspends the script
   (`lua_break`), the ticks stop, and the editor stays live. Where a Rust call sits
   between two scripts on the stack and the script cannot be suspended, the game thread
   waits while the editor's frames go on (ADR 0107 decides how).
7. **One core, two front ends:** the core lives in `engine_script`. The editor's panel
   calls it directly; a DAP server on localhost serves VS Code.
8. **Cost:** nothing without breakpoints. A function with a breakpoint runs interpreted,
   5–6× slower on vector code (ADR 0032's measurements), until it is reloaded.

## What we do not take

- A separate debugger program (old Defold's ZeroBrane).
- A remote inspector separate from the editor's own (Godot): ours is the live one.
