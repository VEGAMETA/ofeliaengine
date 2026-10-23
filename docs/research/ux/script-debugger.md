# Script debugger (E9)

Checked: 2026-10-23. Products were studied as products, from their documentation and release
notes; facts marked * come from forums, issue reports or use, not from the vendor's
documentation. Godot (MIT) was read for ideas only (`core/debugger/remote_debugger.cpp`,
master). Luau's debug interface was read in the sources we build (`luau0-src`
0.21.0+luau736) for this note's first version (2026-10-17). No code was carried over. Input
for [ADR 0107](../../adr/0107-editor.md)'s E9; it replaces the note of 2026-10-17.

CLAUDE.md §10: breakpoints, stepping, watch and call stack, built into the editor and for VS
Code; script errors never crash the game in the editor. §12: keyboard first, errors next to
their cause, minimal modes, immediate feedback. ADR 0107 §6 fixed the core: one debugger in
`engine_script`, a DAP server for VS Code, the game paused and the editor live, nothing paid
without breakpoints. ADR 0114 §9: with the debugger, "pause on errors" stops at the failing
line.

## How they do it

### The debugger

| Tool | Breakpoints and marks | Stepping and keys | Values while stopped |
|---|---|---|---|
| Godot 4.7 ([panel](https://docs.godotengine.org/en/stable/tutorials/scripting/debug/debugger_panel.html), [tools](https://docs.godotengine.org/en/stable/tutorials/scripting/debug/overview_of_debugging_tools.html), [keys](https://docs.godotengine.org/en/4.4/tutorials/editor/default_key_mapping.html)) | a red dot in the gutter, kept across restarts; the `breakpoint` keyword writes one into the script; "Skip all breakpoints"; no conditions or logpoints ([proposal #909](https://github.com/godotengine/godot-proposals/issues/909), open since 2020) | F9 toggle, F10 over, F11 into, F12 continue; Step Out came in 4.6, on Alt+F11, since Shift+F11 is full screen ([#97758](https://github.com/godotengine/godot/pull/97758)); a green arrow at the stopped line | the Debugger panel opens by itself: frames, then Locals, Members (`self` first) and Globals, editable; an Evaluator of expressions, only while stopped |
| VS Code ([debugging](https://code.visualstudio.com/docs/debugtest/debugging), [keys](https://code.visualstudio.com/docs/reference/default-keybindings)) | red dot; grey when off; hollow grey when the adapter could not place it; conditions, hit counts, "wait for breakpoint"; logpoints (diamonds, `{expr}` in the text); function and data breakpoints | F5 continue, F6 pause, F10, F11, Shift+F11, Shift+F5 stop, Ctrl+Shift+F5 restart; a floating tool bar (`debug.toolBarLocation`: floating, docked or hidden) | Variables by scope (Set Value), Watch evaluated at every stop, Call Stack, Breakpoints; values on hover; the Debug Console (Ctrl+Shift+Y) evaluates in the selected frame |
| Roblox Studio ([debugging](https://create.roblox.com/docs/studio/debugging)) | a red circle in the gutter, hollow when off, a middle click deletes; conditions, logpoints ("Continue Execution"), "Remove Breakpoint on Hit", "Trigger At" client, server or edit | F11 into, F10 over, Shift+F11 out; a Resume button; a yellow arrow at the next line | Watch (Variables, My Watches), Call Stack and Breakpoints windows; hover shows tables; a name out of scope reads "void", "which does not mean nil"* ([guide](https://devforum.roblox.com/t/using-the-debugger/2180216)) |
| Unity 6 with Rider or Visual Studio ([Unity](https://docs.unity3d.com/Manual/managed-code-debugging.html), [Rider](https://www.jetbrains.com/help/rider/Debugging_Unity_Applications.html), [Visual Studio](https://learn.microsoft.com/en-us/visualstudio/gamedev/unity/get-started/using-visual-studio-tools-for-unity)) | the IDE's (conditions, hit counts, tracepoints); Rider's **pausepoints** pause the Unity Editor "at the end of the current frame" instead of stopping the code | the IDE's keys; "Attach to Unity and Play" attaches and plays on F5; a list of running instances (project, machine, editor or player, port) | the IDE's; Rider lists the scene, its GameObjects and their components among the variables |
| JetBrains IDEs ([breakpoints](https://www.jetbrains.com/help/idea/using-breakpoints.html)) | click or Ctrl+F8; **Shift+click makes a logpoint**, Alt+click turns one off and on; conditions, pass counts, "remove once hit", "disable until" another is hit; **instance filters**; Mute Breakpoints | the IDE's | values beside the code*, Set Value, Evaluate |
| Unreal 5.8, Blueprints ([debugger](https://dev.epicgames.com/documentation/en-us/unreal-engine/blueprint-debugger-in-unreal-engine), [example](https://dev.epicgames.com/documentation/en-us/unreal-engine/blueprint-debugging-example-in-unreal-engine)) | a red octagon on a node (F9*), outlined when off*, a yellow "!" where it cannot stop; kept in the project's `.ini`, not shared | stop, resume and steps in the tool bar while stopped; a large red arrow on the node | "Watch this value" on a pin; a pin holds what it computed the last time its node ran; the Blueprint Debugger lists breakpoints, watches and the execution trace; "Active Wires" pulse while it runs |

### The game while stopped

| Tool | The game and its time | The editor | An error during play | Many instances of one script |
|---|---|---|---|---|
| Godot | the game's process waits in its debug loop: its window keeps the last frame, its input is dropped, a captured mouse is shown, and the editor may take the focus (read in the source) | usable; the Remote tree answers while stopped | stops at the line as a breakpoint does, the panel open; a toggle ignores error breaks (2025*) | the instance that runs first stops; `self` is among Members; no filter |
| Roblox Studio | a stop, like Pause Scripts, pauses every script and the rendering* ([post](https://devforum.roblox.com/t/pausing-physics-in-studio-beta/2925013)) | the docs do not say | to the Output with its stack, the game goes on; pausing on errors is a mode (Never, Always, Unhandled: [enum](https://create.roblox.com/docs/reference/engine/enums/DebuggerExceptionBreakMode)) whose button users lost after a ribbon change* ([2025](https://devforum.roblox.com/t/where-is-debug-stop-on-unhandled-exceptions/3654417)) | a breakpoint set while playing reaches the script's clones, one set while editing does not ([ScriptDebuggerService](https://create.roblox.com/docs/reference/engine/classes/ScriptDebuggerService)) |
| Unity + IDE | the Editor "becomes unresponsive at breakpoints until you choose the continue option in your IDE"; debugging needs the slower Debug code mode, switched beforehand; after a long frame `deltaTime` is clamped to `maximumDeltaTime` ([reference](https://docs.unity3d.com/ScriptReference/Time-maximumDeltaTime.html)), so the frame after a stop still jumps by up to that much | frozen; Rider's pausepoints keep it live | to the Console, the game goes on; "Error Pause" pauses Play when a script calls `Debug.LogError` ([Console](https://docs.unity.com/en-us/engine/6000.3/manual/unity-editor/editor-windows-views-reference/console-window/console.md)) | conditions in the IDE |
| Unreal (Blueprints) | Play pauses before the node runs; the editor opens the graph at the node | the docs do not say what works meanwhile | runtime errors go to the Message Log, naming the Blueprint, function, graph and node; users call it spam* | the Debug Object drop-down picks the instance whose flow is shown and whose breakpoints stop; users report it set by itself, the other copies skipped* |

**What works:**
- F9, and F10, F11, Shift+F11 nearly everywhere (VS Code, Visual Studio, Roblox; Godot differs
  only for Out).
- Godot: the stop and the code in one window. The panel opens by itself, the line is marked,
  `self` is among the variables, and an error stops at its line.
- VS Code: the four parts (Variables, Watch, Call Stack, Breakpoints); conditions, hit counts,
  and logpoints whose `{expr}` is Luau's own interpolation syntax.
- Roblox: conditions and logpoints in an engine's own Luau debugger; a breakpoint's context.
- Rider: pausepoints (Play pauses at the frame's end, the editor stays live); scene objects
  among the variables.
- JetBrains: Shift+click for a logpoint; instance filters; Mute.
- Unreal: the stopped node opened and marked; a pin's last value; one instance picked to
  watch, and a magnifying glass that finds it in the level.
- Visual Studio for Unity: one key attaches and plays; a list of running instances.

**What annoys:**
- Unity: the editor freezes at a stop; debugging needs a slower code mode chosen beforehand.
- Unreal: an instance filter the editor sets by itself silently skips the other copies*.
- Godot: no conditions or logpoints six years after the request; F12 continues (ours goes to
  a definition); the running game is seen through a second, remote tree.
- Roblox: pausing on errors is a hidden mode; out-of-scope values read "void"; a change to
  the breakpoint that holds the game applies only to the next session; clones get a
  breakpoint or not by where it was set.
- VS Code: the floating tool bar sits over the tabs (a
  [CSS hack](https://www.eliostruyf.com/css-hack-visual-studio-code-move-debug-toolbar/) moves
  it); each debugger needs an extension that names its type
  ([configuration](https://code.visualstudio.com/docs/debugtest/debugging-configuration)).
- A stop lands in the middle of a frame: some objects ran their code this frame, others did
  not. None of the documentation read says so.

## What Luau gives us

Read 2026-10-17, corrected 2026-10-23 by `docs/research/script-debugger.md`, which has the
details and how a stop suspends the stage (ADR 0119).

- **Breakpoints:** `lua_breakpoint(L, funcindex, line, enabled)` puts `LOP_BREAK` over the
  line's first instruction, or the next line with code, in the function and those nested in
  it. The `debugbreak` callback runs when it is hit.
- **Stepping:** with `lua_singlestep` on, `debugstep` runs before every instruction. The
  flag is read when the interpreter is entered, so a step from a stop is a `lua_break` and a
  resume.
- **The stack and values:** `lua_getinfo`, `lua_stackdepth`, `lua_getlocal` and
  `lua_setlocal` (locals can be changed), `lua_getupvalue`. Names need debug level 2.
- **Errors:** `debugprotectederror` runs only where Luau can yield, never under the host's
  `lua_pcall`. A stop at an error reads the stack `lua_resume` leaves when it fails.
- **Pausing without freezing:** `lua_break` suspends the script like a yield; the frame goes
  on; resuming continues at the breakpoint. It cannot cross a C call (a `table.sort`
  comparison, a metamethod written in Luau).
- **Native code has no breakpoints:** a function with one runs interpreted until reloaded.

## Scenarios

| The developer wants to… | Ours | Count | Elsewhere |
|---|---|---|---|
| stop on a line during Play | F9 on the line (or a click in the gutter), F5 | 2 | Godot, Roblox 2; Unity: Debug mode, attach, Play, then the editor freezes |
| step through a hook, watching a prop | F10 per line; values beside the lines and in Variables, the changed ones lit; the game's view redrawn after each step | 1 a line | VS Code-like everywhere; Unity's scene cannot be looked at meanwhile |
| follow one entity across ticks | at a stop, "Only Front door" in the panel's header; F5 per tick | 1, then 1 a tick | a condition typed by hand; Unreal: the Debug Object |
| see which entity stopped and its components | the header names it; Variables shows `self`: props, then its components, live; a click on the name selects it in the scene | 0 (1 to select) | Godot: Members, then the Remote tree; Unreal: the Debug Object |
| stop only for "Front door", before playing | Alt+F9 on the line, Tab to "Only for", `@front`, Enter (the selection is offered first); or the condition `self.entity.name == "Front door"`, completed as typed | 3 + typing | VS Code, Roblox: right click, Condition, type; Godot: edit the script |
| see why a script failed during Play | nothing: the game stops at the failing line, the editor in front, the message at the line, the stack and values below; fix, Ctrl+S, F5 | 0 to see, 2 to go on | Godot 0; Unity and Roblox: a red row to click, the game goes on |
| find a loop that never ends | nothing: the 50 ms budget's error stops in the loop, at its line | 0 | Roblox: a timeout error in the Output* |
| log a value without stopping | Shift+click the gutter, type `speed = {self.speed}`, Enter: the line's end shows the latest, the Output a row per hit | 2 + typing | VS Code: right click, Add Logpoint, type; Godot: `print`, save |
| look around the stopped world | Shift+F8: the editor's camera and tools on it, edits allowed; F5 goes on | 1 | Unity: frozen; Godot: the Remote tree |
| fix a value and go on | F2 on it in Variables (or the inspector), type, Enter, F5 | 2 + typing | VS Code: Set Value |
| debug from VS Code | once: install the recommended extension; then F5 on "Attach and Play": breakpoints shared both ways | 1 | Godot: godot-tools and `debugServer: 6006`; Unity: attach, then Play in Unity |
| debug the standalone player | `ofeliaengine run --debug`, then F5 on "Attach to the player" in VS Code | 2 | Unity: Development Build and Script Debugging, attach by IP |

## What we take

### 1. The gutter

- **A column of its own,** left of the line numbers.
- **Marks,** by shape first, so color is never the only sign:
  - ● a breakpoint (red); ◆ a logpoint; ‖ a pause point, which pauses the game after its tick
    (Rider's pausepoint);
  - the same shapes hollow: off;
  - a short bar inside: a condition, a hit count or an entity filter;
  - grey with a slash: every breakpoint skipped ("Skip breakpoints");
  - a yellow arrow and a band: the line about to run; a hollow arrow: a caller's line picked
    in the Call Stack;
  - stopped by an error: the band and the message in the error color, as findings show.
- **A breakpoint on a line without code** moves to the next line with code at once (Luau's
  rule; scripts are loaded in edit mode too, so it is known before Play).
- **The mouse:** a click sets or removes; Shift+click makes a logpoint and opens its field
  (JetBrains); Alt+click turns it off or on (JetBrains); a middle click removes (Roblox); a
  right click opens its options. A faint dot shows where a click would put one*.
- **Breakpoints follow their lines** as the text is edited, survive reloads (re-applied to
  the new functions) and live in the session (`.ofelia/`), never in scripts or scenes.
- **A logpoint's latest message stays at its line's end** while the game runs, with its
  count: `speed = 2 · Front door ×312`. Unreal's pin that shows its last value, for text.

### 2. The keys

| Keys | What | Whose |
|---|---|---|
| F9 | a breakpoint on the caret's line, or none | VS Code, Visual Studio, Godot, Unreal* |
| Shift+F9 | a logpoint on the caret's line, its field open | ours (Shift+click is JetBrains') |
| Ctrl+F9 | the line's breakpoint off or on | Visual Studio |
| Alt+F9 | the line's breakpoint's options | ours (Visual Studio: Alt+F9, C) |
| Ctrl+Shift+F9 | remove every breakpoint ("Debug: Restore Removed Breakpoints" brings them back) | Visual Studio, Godot |
| F5 | continue, while stopped or paused; play, while editing | VS Code, Visual Studio |
| F10, F11, Shift+F11 | step over, into, out | VS Code, Visual Studio, Roblox |
| Ctrl+F10 | run to the caret's line | Visual Studio |
| Shift+F7 | while stopped: finish the tick, then pause | ours (E3's step) |
| Ctrl+Shift+D | the Debugger panel | VS Code (Run and Debug) |
| F7, F8, Ctrl+F5, Shift+F8 | pause, stop, restart, eject, as since E3 | ours |

- **Not taken:** F6 (VS Code's Pause; `play-mode.md` keeps it for playing the open scene),
  F12 (Godot's Continue; ours is Go to Definition), Shift+F5 (VS Code's Stop; ours plays from
  here, and F8 stops), Ctrl+F5 (VS Code's Run Without Debugging; ours restarts). There is no
  "run without debugging": with no breakpoints the debugger costs nothing.
- **"Break at the Next Script Line"** (Godot's Break, VS Code's Pause) is a palette command
  without a key: F7 pauses the game, and the 50 ms budget's error already catches a loop.
- **The keys work in the game's window too** (F1 to F12 stay the editor's while playing).
- **F10 is Windows' menu key.** `engine_platform` hands system keys to `DefWindowProcW`
  (`window.rs`: "Let the system handle Alt+F4 and the window menu"), where F10 starts the
  window menu's loop (games report their loop pausing:
  [gamedev.net](https://gamedev.net/forums/topic/644254-pressing-f10-pause-main-loop/)). E9
  keeps F10 from it.

### 3. The panel

**Debugger,** a bottom tab after Output, as the layout planned (`editor-layout.md`).
Ctrl+Shift+D opens it. It opens by itself at a stop (Godot) and stays open after.

```text
┌ Output · Debugger ● · Tasks · Profiler ──────────────────────────────────────────────┐
│ ⏵ F5  ⤼ F10  ↓ F11  ↑ ⇧F11 │ Breakpoint · Door.on_tick · Front door · tick 1,284 │
│                            │ Only Front door · Copy                  ☐ Skip breakpoints │
├ CALL STACK ───────────┬ VARIABLES ────────────────────────────┬ BREAKPOINTS ──────────┤
│ ▶ Door.on_tick    :14 │ WATCH  +                              │ ☑ errors not caught   │
│   (Front door)        │   self.speed               2          │ ☐ errors pcall caught │
│   steer.lerp_to   :31 │ LOCALS                                │ ● door:14 Front door  │
│   engine              │ ▾ self  Door · Front door  Instances  │ ◆ door:20 speed = {…} │
│                       │     lift 3  speed 2  closed (4, 0, 0) │ ○ coin:8              │
│                       │   ▸ Transform  translation (4, 1.2, 0)│                       │
│                       │   dt 0.015625                         │                       │
│                       │ ▸ UPVALUES  ▸ GLOBALS                 │                       │
│                       │ > evaluate in Door.on_tick            │                       │
└───────────────────────┴───────────────────────────────────────┴───────────────────────┘
```

- **The header:** the stop's reason, its hook, its entity and the tick; Continue, Over, Into
  and Out with their keys; "Only Front door" (limits the breakpoint that stopped); Copy (the
  message and the stack as text, Godot's); "Skip breakpoints", a mode shown on every dot.
  The step buttons live only here, and the tab row's ▶ turns into Continue: no floating tool
  bar.
- **Call Stack:** Luau frames only, the hook's frame naming its entity, the engine below as
  one dim line. A click shows that frame's line (the hollow arrow) and its values.
- **Variables:**
  - **Watch** first: expressions evaluated at every stop in the selected frame; "not in scope
    here" instead of a value (not Roblox's "void");
  - **Locals,** then **Upvalues** and **Globals,** both closed by default;
  - **`self` open:** its props, then its entity's components, `entity`, `locals`. Entities
    show their name and open into their components. Tables open in chunks of 100;
  - **F2 or a double click** changes a value. A prop or a component's field is written as the
    inspector writes it (clamped, an undo step, kept with Ctrl+K); a local is not an undo step;
  - **values changed since the last stop are lit** (VS Code, Rider*);
  - **Enter on an entity** selects it in the scene; **Instances** on `self` shows the tree
    filtered by its component (`:Door`, the tree's filter).
- **Evaluate,** the last line of Variables: one line of Luau in the selected frame (Godot's
  Evaluator, VS Code's console). While paused between ticks it has `world` and the
  libraries. The answers stand above it; Up and Down recall earlier lines.
- **Breakpoints:**
  - first, "Errors the script does not catch" (on) and "Errors caught by pcall" (off), like
    VS Code's error filters and Roblox's Unhandled and Always modes;
  - then each breakpoint: its mark, `file:line`, the line's code dimmed, its options in words
    ("only Front door", "hits ≥ 10", the log's text) and its hits since Play;
  - Space turns it off or on, Delete removes, Enter goes to the line, F2 edits.
- **The panel is a list like the others:** arrows move, Left and Right close and open rows,
  every action is a command in the palette.

### 4. Values in the code

- **While stopped,** the lines above the stopped one in its function show values at their
  end: the locals assigned there and the props written (`self.speed = …`). They are dimmer
  than the code, and the changed ones are lit (VS Code's and Rider's inline values*).
- **Hovering a name** while stopped shows its type and value (`speed: number = 2`); tables,
  entities and components open in the card.

### 5. How a stop looks

- **The script's tab comes to the front** at the line (opened if it was closed); its tab
  carries the arrow.
- **The editor's window takes the keyboard** (Godot's habit). The game's
  window loses it, which frees the mouse (our rule since E3; Godot does the same). Continue
  gives both back as they were.
- **The viewport's green "playing" edge turns amber** with the word "Stopped". The game's
  window is titled `main.scene (stopped at door.luau:14)`. The status bar says where and
  which keys go on (F5, F10, F11).
- **The scene tree** (in the scene's tab, or beside the code with View: Scene Docks Beside
  Code) marks the stopped entity's row with the arrow and scrolls to it. The selection does
  not change; a click on the name in the header selects it.
- **The header says the tick is half done** ("tick 1,284, under way"): the hooks before this
  one ran, the rest did not. Shift+F7 finishes it.
- **The inspector,** on that entity, says "Stopped at line 14" on the script component's
  header.
- **The whole editor works meanwhile:** select, inspect, edit (play edits, kept with Ctrl+K),
  Shift+F8 to fly around, Ctrl+S on a script. Nothing in the world changes but by your edits
  and your steps.
- **The rare case:** stopped in a function the engine's C code called (a `table.sort`
  comparison, a metamethod written in Luau), the script cannot be suspended. The world is
  read-only until Shift+F11 leaves the function; the header and the inspector show a lock
  and say why (ADR 0107 §6).

### 6. The game's time while stopped

- **The ticks stop:** `time.now()`, timers, tweens, sounds and videos stand, as with F7.
- **Continue finishes the stopped tick** (the hooks after it, the game's `tick`, physics),
  then the ticks go on at their rate. The time spent stopped is never caught up: no burst of
  ticks, no jump in `dt` (Unity's clamped jump is what we avoid).
- **The 50 ms budget** of a hook does not count the time stopped. The profiler leaves frames
  spent stopped out.
- **The views draw only when something changes,** a step or an edit, so stepping over
  `t.translation += …` moves the door on screen.

### 7. Stepping

- **F10 over, F11 into, Shift+F11 out.** F11 into a `require`d script's function opens its
  tab; the engine's functions are stepped over.
- **Past a hook's end,** a step stops at the next script line that runs: a message's receiver
  (messages are delivered when the hook returns), the next instance's hook, a system, or the
  next tick's first hook. The header names the new entity.
- **Ctrl+F10** runs to the caret's line.
- **Shift+F7 while stopped** finishes the tick and pauses the game: a whole world, every
  hook of the tick done.
- **Saving a script while stopped in it:** the stopped call finishes in the old code (its
  arrow hollow, "old code until it returns"); the next call runs the new code.

### 8. A breakpoint's options

- **Alt+F9, or a right click,** opens one line under the code (VS Code's inline editor, on
  our one-line field). Tab moves between its parts, Enter keeps, Escape drops:
  - **Condition:** a Luau expression in the line's scope (`self.hp < 0`), completed and
    checked as you type by the editor's Luau analysis;
  - **Hits:** `10` (the tenth), `>= 10`, `% 64` (every 64th: once a second in `on_tick`);
  - **Only for:** entities, found as `@` finds them in the palette, or the selection. Scene
    entities are kept by scene id; one the game spawned is kept until Stop (JetBrains'
    instance filter, from a stop's header);
  - **When hit:** Stop, Pause the game after this tick (Rider's pausepoint), or Log only;
  - **Log:** text with `{expressions}`: Luau's own interpolation, and VS Code's syntax.
- **A condition that fails at run time** stops at its breakpoint with its error, and the dot
  takes a warning mark: skipping it silently would hide the mistake.

### 9. Errors during Play

- **Pause on errors,** on by default in the editor: an error a script does not catch stops
  the game at the failing line, before the instance stops. The message stands at the line's
  end in the error color, the stack and values in the panel, the error badge on the entity's
  row.
- **Once per line per Play:** after Continue, the same line's errors are counted in the
  Output's row and do not stop again. A hundred coins with one bug stop once.
- **The 50 ms budget's error** stops where the time ran out, inside the loop.
- **Continue lets the error take its course:** the instance stops (ADR 0114 §9). Ctrl+S
  starts the stopped instances again (E7-4b); Ctrl+F5 restarts Play.
- **Off, or outside Play:** ADR 0114 §9's way: the Output's row with its cause, the entity's
  badge, "Stopped: … — Resume" in the inspector.

### 10. Many instances of one script

- **A breakpoint in a component's hook stops for every instance,** in the fixed order (the
  order the entities were made in, as the hooks run).
- **The stopped instance is the one shown:** the header names its entity, `self` is it, and
  watches evaluate against it.
- **"Only Front door"** in the header limits the breakpoint that stopped: one click.
- **Systems have no `self`:** the header names the system, a loop's `e` shows its entity's
  name, and a condition filters (`e.name == "Front door"`).
- **A filter is always in sight,** on the dot and in the list, and the editor never sets one
  by itself (Unreal's Debug Object is what we avoid).

### 11. VS Code and the standalone player

- **One core, two front ends** (ADR 0107 §6). The editor's panel and gutter call the core; a
  DAP server on 127.0.0.1 serves VS Code. Both see one set of breakpoints; a change in either
  shows in the other (DAP's `breakpoint` events). On attaching, the two sets join.
- **VS Code needs an extension** that names the debugger's type (VS Code's rule). Ours finds
  the project's running editor and players in `.ofelia/debug/`, where each writes its port
  and a token: no port to type, and a pick only when several run (Visual Studio's list of
  Unity instances).
- **`ofeliaengine new` writes `.vscode/launch.json`:** "Attach to the editor", "Attach and
  Play" (F5 plays the scene in the editor, Shift+F5 stops it: Visual Studio's "Attach to
  Unity and Play") and "Attach to the player"; `.vscode/extensions.json` recommends the
  extension.
- **While VS Code is attached,** a stop shows in both, and VS Code takes the focus, not the
  editor.
- **VS Code's Pause (F6)** stops at the next script line that runs; with no script running,
  it pauses the ticks as F7 does.
- **The standalone player** (a development build) listens only when started with `--debug`;
  `--wait-for-debugger` holds it before the first hook (Unity's "Wait For Managed
  Debugger"). Stopped, its window's title says where, and the mouse is freed. Shipping builds
  have no debugger.
- **Only this machine connects,** with the token: an evaluation runs code in the game.

### 12. Costs

- **Nothing without breakpoints:** hooks run native, nothing single-steps (ADR 0107 §6).
- **A function with a breakpoint runs interpreted** (5–6× slower on vector code) until it is
  reloaded. Removing a module's last breakpoint reloads it (0.2–0.3 ms, E7-4b), so it runs
  native again.
- **A condition** costs one interpreted expression per hit. **A logpoint** formats its values
  into the log's ring and makes no Luau string, so a frame still allocates nothing.
  Measured in E9.
- **A stop shows** in the frame its line runs in; a step's line in the next frame.
- **The panel** builds only the rows in view and reads a value only when its row is open
  (DAP's variable references); nothing allocated per frame (ADR 0107 §8).
- **Every action is a command** (`debug.toggle_breakpoint`, `debug.step_over`, …), so the
  palette, the key maps and the MCP layer (5a) reach all of it.

## What we do not take

- A frozen editor at a stop, and a code mode to switch before debugging (Unity).
- An instance filter the editor sets by itself (Unreal).
- A second, remote tree for the running game (Godot).
- A floating tool bar (VS Code).
- Breakpoints that act differently by where they were set (Roblox's edit and play).
- Godot's F12 for Continue and Alt+F11 for Step Out.
- A separate errors tab (Godot): the Output is the one list (`output.md`).
- **Later:** stopping when a prop changes (a script's write would stop at its line); a
  breakpoint that waits for another; looking back from a stop at the last ticks (saves copy
  10,000 entities in 0.023 ms, ADR 0104, so a ring of ticks is plausible); co-op in one
  process, where one stop pauses every world.

## Steps

1. **E9-1, the core and the editor:** breakpoints and their options, logpoints, stepping,
   the stack and values, pause on errors; the gutter, the panel, values in the code; the
   stop's look in the code, the viewport, the game's window and the tree; F10 kept from the
   window menu. Measured: nothing without breakpoints, a stop within its frame.
2. **E9-2, DAP and VS Code:** the server, the extension, `launch.json` and
   `extensions.json` from `ofeliaengine new`.
3. **E9-3, the standalone player:** `--debug`, `--wait-for-debugger`.

## Questions for the team

Settled 2026-10-23 with ADR 0119's acceptance (team decision): every recommendation. The
first two were ADR 0119's questions 3 and 4; the other five are the defaults it takes.

1. **The VS Code extension.** VS Code talks to a debugger only through an extension. Ours is
   small (TypeScript, about a hundred lines: find the running editor or player, connect).
   - a) Publish it on the VS Code Marketplace and Open VSX: one click to install, but it
     needs a publisher account in the team's name.
   - b) Keep it in the repository; `ofeliaengine new` installs it from a `.vsix`
     (`code --install-extension`). No account; it updates with the engine.
   - c) No VS Code debugging in E9; only the editor's own.
   - Recommended: b) now, a) at the first public release.
2. **Pause on errors by default.**
   - a) On: the game stops at a failing line, once per line per Play (Godot's way; ADR 0107
     §6 and ADR 0114 §9 already plan it).
   - b) Off: the error goes to the Output and the entity's badge, and the game goes on
     (Unity's and Roblox's default).
   - Recommended: a).
3. **Breakpoints written in the code.**
   - a) No: breakpoints live in the editor's session only (VS Code, Unreal).
   - b) Also a `breakpoint()` function (Godot's keyword): it stops under the debugger and does
     nothing elsewhere. It can be shared through version control, and forgotten in the code.
   - Recommended: a).
4. **The standalone player.**
   - a) A development build listens for the debugger only with `--debug`: nothing is open by
     default.
   - b) Every development build listens (Unity's Script Debugging, ticked).
   - Recommended: a).
5. **Stepping past the end of a hook.**
   - a) It stops at the next script line that runs, whichever entity's (Godot's way): a
     message's receiver, the next instance, the next tick.
   - b) It goes on to the next breakpoint (stepping out of the last frame elsewhere).
   - Recommended: a).
6. **`self.locals` while stopped.** ADR 0114 §8 shows it read-only while paused.
   - a) Writable in the debugger's Variables (it is scratch data, never saved), read-only in
     the inspector.
   - b) Read-only in both.
   - Recommended: a).
7. **A network session while stopped** (co-op tests): the transport drops a peer after 10 s
   without packets (ADR 0070).
   - a) Keep the session alive while stopped; the peers' games wait.
   - b) Let it time out, as a real stall would.
   - Recommended: a), when co-op in the editor comes.
