# Script debugger: stopping a script, Luau's debug interface, DAP

Checked: 2026-10-23. Input for ADR 0107's E9 (the core, DAP for VS Code, the panel and
the gutter) and ADR 0114. It sits under the UX note `docs/research/ux/script-debugger.md`
(2026-10-17), which chose what the developer sees. This note is about how.

The questions:
- what Luau 0.736 (the one mlua 0.12.1 builds) gives a debugger, how it meets native code,
  and what mlua exposes of it;
- how other Lua and Luau debuggers, and Godot, stop a script and talk to an IDE;
- what VS Code needs over DAP;
- how to stop a script when Play's ticks and the editor's UI run on one thread;
- what it costs.

No code was carried over. luau-debugger's debugger has no license, and the Defold License
is not on our list (CLAUDE.md §15): both were read for ideas only. Godot,
local-lua-debugger-vscode and MobDebug (MIT) and EmmyLuaDebugger (Apache-2.0) were read the
same way. Luau and mlua are already ours (MIT): their functions are named here, not copied.

## Sources

- **Luau 0.736** as `luau0-src` 0.21.0+luau736 vendors it (MIT; the version in our
  `Cargo.lock`), read in the cargo registry's copy:
  - `VM/include/lua.h` (the debug API, `lua_Callbacks`); `VM/src/ldebug.cpp`, `ldo.cpp`,
    `lvmexecute.cpp`, `lcorolib.cpp`, `lapi.cpp`, `lstate.cpp`;
  - `CodeGen/src/CodeGen.cpp` (`onDisable`) and `CodeGenContext.cpp` (`onEnter`,
    `setNativeExecutionEnabled`);
  - `Compiler/include/luacode.h` (debug and optimization levels).
- **Luau's conformance tests** at tag 0.736 (MIT): `tests/Conformance.test.cpp`, the
  "Debugger" and "InterruptInspection" cases, which drive the API the intended way.
  https://github.com/luau-lang/luau/blob/0.736/tests/Conformance.test.cpp
- **mlua 0.12.1 and mlua-sys 0.12.0** (MIT): `src/state.rs`, `src/function.rs`,
  `src/thread.rs`; mlua-sys `src/luau/lua.rs`, `src/luau/luacodegen.rs`.
- **luau-debugger** (sssooonnnggg), master at `58bb3b6` (2026-04-07):
  https://github.com/sssooonnnggg/luau-debugger
  - no license for the debugger or its runtime (none at the root, none in `debugger/`);
    only its VS Code extension (0.2.3) is MIT.
- **local-lua-debugger-vscode** (tomblind), `d51ee29` (2022-04-03), MIT; Lua 5.1 to 5.3 and
  LuaJIT. https://github.com/tomblind/local-lua-debugger-vscode
- **MobDebug** 0.805 (pkulchenko), `4cf97f9` (2023-09-24), MIT.
  https://github.com/pkulchenko/MobDebug
- **EmmyLuaDebugger**, `a3b5b47` (2026-10-19): Apache-2.0 in its sources' headers, no
  LICENSE file; Lua 5.1 to 5.5 and LuaJIT, not Luau. https://github.com/EmmyLua/EmmyLuaDebugger
- **Defold** (Defold License 1.0: Apache-2.0's terms plus a clause against selling it as a
  game engine product):
  - its native DAP server, PR 13321 ("Add native Lua DAP debugger to debug engines", merged
    2026-10-06), https://github.com/defold/defold/pull/13321;
  - its editor moved to it from MobDebug, PR 13322 (2026-10-09);
  - `engine/debugger/README.md` and `engine/debugger/src/debugger.cpp` read at `dev`.
- **Godot 4.7.2-stable** (`ed1daf0`, 2026-08-16), MIT, read as code:
  `core/debugger/remote_debugger.cpp`, `script_debugger.h`, `engine_debugger.h`,
  `remote_debugger_peer.cpp`; `modules/gdscript/gdscript_vm.cpp` (`OPCODE_LINE`);
  `editor/debugger/debug_adapter/`.
- **DAP:** the specification's overview,
  https://microsoft.github.io/debug-adapter-protocol/overview, and VS Code's debugger
  extension guide, https://code.visualstudio.com/api/extension-guides/debugger-extension.
- **Tracy 0.14.1's client** in tracy-client-sys 0.30.0 (BSD-3-Clause):
  `client/TracyProfiler.hpp`, for what `TRACY_FIBERS` changes.
- **Ours:** `engine_script/src/lib.rs`, `engine_app/src/scripts.rs`, `script_views.rs`,
  `player.rs`; `engine_editor/src/app.rs` (`run_inner`, `EditorGame`); ADR 0032's and ADR
  0114's measurements.

## Luau's debug interface (0.736)

### Breakpoints

- `lua_breakpoint(L, funcindex, line, enabled)` moves the line to the next line with code
  and returns it (-1: none). That is the DAP breakpoint's verified line.
- It patches one byte: the opcode of the first instruction of that line becomes
  `LOP_BREAK`, in the function and in every function nested in it.
  - A function keeps a copy of its original opcodes (`debuginsn`), made at its first
    breakpoint.
  - Closures share their function's bytecode: a breakpoint set through a module's chunk
    reaches every closure made from it, old ones included.
- On `LOP_BREAK` the interpreter calls `debugbreak`, then runs the original instruction.
  A breakpoint whose condition is false costs one callback.

### Stepping

- `lua_singlestep(L, on)` sets a flag on one thread. New threads copy it
  (`lua_newthread`, `coroutine.create`).
- **The flag is read when the interpreter is entered.** `luau_execute` picks its stepping
  loop or its plain loop then. A loop already running keeps its mode, and a call from Luau
  to Luau stays in the same loop.
  - So stepping turned on inside `debugbreak` does not start when the callback returns.
    It starts at the next entry: a `lua_resume`, or a call from C.
  - In the stepping loop, `debugstep` runs before every instruction (but BREAK and the
    vararg prologue), and native code is never entered.

### Breaking and resuming

- `lua_break(L)`, from a callback, suspends the thread: `lua_resume` returns `LUA_BREAK`.
  - Resuming runs the BREAK instruction again, so the callback comes again at the same
    place and must let it pass once. The conformance test breaks "on the first invocation
    and continue[s] on second".
- It is legal only where `lua_isyieldable(L)`. Elsewhere it raises "attempt to break across
  metamethod/C-call boundary".
  - **Yieldable:** code a host started with `lua_resume`, through calls from Luau to Luau,
    generic `for` iterators, `pcall`, and calls from C functions that have a continuation
    (`lua_pushcclosurek`, `lua_pcallyieldable`).
  - **Not yieldable:** anything under a host's `lua_pcall`; metamethods written in Luau;
    callbacks of C library functions (`table.sort`'s comparator, `string.gsub`'s
    function); C functions without a continuation. Ours: `self:open()` goes through
    `self_namecall`, which calls `Door.open` with `lua_call`.
- When a coroutine a script resumed breaks, `debuginterrupt` runs on its parent and the
  parent breaks too. Continuing resumes the child first.
- `lua_callhook(L, hook, ud)` runs a hook on a broken or yielded thread as if at the break:
  where a host evaluates in a broken frame.
- `lua_resetthread` clears a broken or failed thread for reuse.

### Errors

- `debugprotectederror` runs when a protected call catches an error, before unwinding,
  **only in a yieldable context**.
  - Under a host's `lua_pcall` it never runs: at the error, `nCcalls` is above
    `baseCcalls`.
  - It does run for a script's own `pcall` inside a thread started with `lua_resume`
    ("break on caught errors").
- After `lua_resume` returns an error, the thread's frames are still there
  (`resume_finish` does not unwind them). Its stack and locals can be read, then
  `lua_resetthread`.
- Under `lua_pcall`, the error function runs at the failing point, before unwinding.
  Defold's host calls its debugger there.

### The stack and variables

- `lua_stackdepth(L)` counts frames, C ones included.
- `lua_getinfo(L, level, what, ar)`: `s` the source and `linedefined`, `l` the current
  line, `n` the name, `u` upvalues, `a` parameters, `p` function ids, `f` pushes the
  function.
- `lua_getlocal`/`lua_setlocal(L, level, n)`: the n-th local alive at the frame's
  instruction. Names exist only at debug level 2.
- `lua_getargument(L, level, n)`: parameters and varargs, without names.
- `lua_getupvalue`/`lua_setupvalue(L, funcindex, n)`: an upvalue's name is `""` below
  debug level 2.
- **None of these reads a native frame.** They return nothing for frames running native
  code ("changing tables in native functions externally may invalidate safety contracts").
- `lua_debugtrace(L)`: a text trace in a static 4 KB buffer, not thread-safe. For logs.
- Coverage (`lua_getcoverage`) needs `coverageLevel` 1 or more at compile time (an
  instruction per statement). `lua_getcounters` reads native code's counters. The debugger
  needs neither.

### Compiler options (`luacode.h`)

- Debug level 2 is "full debug info with local & upvalue names; necessary for debugger".
  `ScriptVm` compiles at 1.
- Optimization level 2 "includes optimizations that harm debuggability such as inlining".
  `ScriptVm` compiles at 2; level 1 "doesn't prevent debuggability".

### Native code

- Code generation can stay on. `lua_breakpoint` on a natively compiled function calls the
  code generator's `onDisable`:
  - the function's entry goes back to its bytecode;
  - every live frame of it, in every thread, loses its native mark;
  - from then on it runs interpreted.
- Clearing the breakpoint does not bring native code back ("this will be addressed in the
  future"). For us that lasts until the script is saved (a reload) or Play starts again (a
  new VM).
- Its callers stay native. Their lines are right (native code stores the instruction before
  a call); their locals cannot be read.
- `Luau::CodeGen::setNativeExecutionEnabled(L, false)` sends every new entry to the
  interpreter, and `true` brings native code back. It is C++ only, not in `luacodegen.h`,
  and mlua does not expose it: a shim of a few lines would.
- Luau's own "Debugger" conformance test runs without native code ("Native code doesn't
  support debugging yet").

### What mlua 0.12 gives

- `mlua::ffi` binds everything above, and `lua_Callbacks` with every field (`interrupt`,
  `debugbreak`, `debugstep`, `debuginterrupt`, `debugprotectederror`, `preresume`,
  `postresume`, and the rest).
- Its own API for Luau: `Lua::set_interrupt`, `inspect_stack`, `traceback`,
  `set_thread_event_callback`, `Thread::reset`.
  - `set_interrupt` takes over the `interrupt` callback, which our time budget already sets
    directly: the debugger extends ours instead.
  - `Lua::set_hook` is not built with the `luau` feature.
- **A call made through mlua cannot be resumed after a break:**
  - `Function::call` is `lua_pcall` with mlua's traceback handler: not yieldable, so
    `lua_break` raises there;
  - `Thread::resume` knows `LUA_OK` and `LUA_YIELD`. `LUA_BREAK` takes its error path, and
    `Thread::status` calls a broken thread `Error`.

  Hooks have to be entered with `ffi::lua_resume` on threads the engine keeps.
- `Lua::enable_jit` decides only whether chunks loaded later are compiled.

## How others do it

### luau-debugger (Luau, DAP)

- **Stops** by blocking the Luau thread inside `debugbreak` or `debugstep`, on a condition
  variable. The DAP server (cppdap) runs on its own thread and hands requests that touch
  the VM to the stopped thread (`executeInMainThread`).
- **Steps** with `lua_singlestep(L, true)` set from the start: the whole VM always runs the
  stepping loop, and `debugstep` is set only while a step is wanted. That is how it steps
  while blocking, since the flag is read at entry. Its runtime compiles at optimization 1
  and debug level 2, without native code.
- **Breakpoints:** `lua_breakpoint` on the chunk of every file the host reports
  (`onLuaFileLoaded`), applied on the VM's thread inside the `interrupt` callback.
  Conditions are evaluated in `debugbreak`.
- **Pause:** a flag read in `interrupt`.
- **Evaluate:** a table of the frame's locals and upvalues over its environment; the text
  compiled (with `return` in front first, then as a statement), `luau_load`, `lua_pcall` on
  the stopped thread. Values are copies; `setVariable` writes back with `lua_setlocal`.
- **DAP:** initialize, launch, attach, setExceptionBreakpoints, setBreakpoints, continue,
  threads (one), stackTrace, scopes (Local, Upvalues, Globals), variables, setVariable,
  next, stepIn, stepOut, evaluate, pause, disconnect. It listens on 0.0.0.0.

### local-lua-debugger-vscode (Lua)

- The debugger is Lua code in the debuggee (`lldebugger.lua`). A line hook
  (`debug.sethook(…, "l")`) checks a table of breakpoints and blocks, reading commands from
  stdin or a pipe.
- DAP lives in the VS Code extension (TypeScript), which talks to the debuggee over its
  stdio with a line protocol. Launch only.
- Evaluate: an environment whose `__index` and `__newindex` reach the locals, then the
  upvalues, then the function's environment. After the statement, `debug.setlocal` and
  `setupvalue` write changed values back.

### MobDebug (Lua)

- Line, call and return hooks (`"lcr"`); breakpoints in a table looked up at each line.
- The debuggee connects to the IDE (ZeroBrane Studio, port 8172) and blocks on the socket
  inside the hook. Its command loop is a Lua coroutine resumed from the hook.
- A break from the IDE: every 200 hook events it reads the socket without blocking.
- Its own text protocol (`SETB`, `STEP`, `OVER`, `OUT`, `RUN`, `EXEC`, `SUSPEND`), not DAP.
  `EXEC` captures the frame's locals and upvalues in a table and restores them after.

### EmmyLuaDebugger (Lua)

- `lua_sethook` (line, call, return). A stop waits on a condition variable while the
  transport's thread queues evaluations for the Lua thread.
- Its own protocol (`MessageCMD`: init, add and remove breakpoint, action, eval, break
  notify) over TCP or a pipe; the IDE plugins talk it.

### Defold's native DAP server (Lua 5.1 and LuaJIT)

- **In the engine** (the game is its own process), off unless asked:
  `--config=debugger.enabled=1`, 127.0.0.1:8172 by default, port 0 for a free one (written
  to the log), `debugger.wait=1` to wait for the client before the first script runs.
- **Stops** by blocking the engine's thread inside the Lua hook:
  `while (paused) { Update(); Sleep(1 ms) }`. The hook (line, call, return, and a count
  every 1,000 instructions) is set only while a client is attached, and LuaJIT's compiler
  is off meanwhile.
- **Breakpoints** stay pending until their line is known from functions it has seen.
  Conditional, hit-count and log breakpoints. Uncaught errors stop "before stack
  unwinding", from the host's protected call's handler.
- **Evaluation** runs on a temporary Lua thread with a time limit (1 s by default).
  Inspection does not run the game's metamethods; userdata go through host callbacks.
- Messages are bounded (1 MiB, queues 4 MiB); a malformed one closes the session and
  resumes Lua; detaching resumes. Release engines leave it out.

### Godot (GDScript)

- **The game runs in its own process** and connects to the editor over TCP
  (`network/debug/remote_port`). A thread moves its messages (`RemoteDebuggerPeerTCP`).
- **Breakpoints:** the editor sends `breakpoint` messages; the game keeps a map of lines to
  sources. GDScript compiles an `OPCODE_LINE` before each line; with the debugger active it
  checks stepping and the map there, and polls messages every 2,048 lines (`line_poll`).
- **A stop** is `RemoteDebugger::debug()`: a loop on the game's thread that flushes output,
  polls messages, answers stack and variable requests, and sleeps 10 ms when idle. On the
  main thread it also drains the window's events (`force_process_and_drop_events`) so the
  window does not hang.
- **Stepping** is two numbers per thread, lines left and depth: step, next and out set
  (depth −1, 1 line), (0, 1), (1, 1); a call moves the depth.
- **Evaluate:** Godot's `Expression` (not GDScript), given the frame's locals, the globals
  and the singletons, `self` as its base.
- **DAP:** the editor serves it on 127.0.0.1:6006 and relays to the game: initialize,
  launch, attach, restart, terminate, configurationDone, pause, continue, threads (one,
  "Main"), stackTrace, setBreakpoints, breakpointLocations, scopes, variables, next,
  stepIn, evaluate. No stepOut.

**What they share:** every one stops by blocking the thread that runs the scripts. Their
editors run elsewhere, in another process (Godot, Defold, ZeroBrane, VS Code), or there is
no editor (luau-debugger's runtime). Ours runs on that thread.

## What VS Code needs (DAP)

- **Messages:** a `Content-Length` header, a blank line, UTF-8 JSON. Requests, responses
  and events.
- **The session** (the specification's overview):
  1. `initialize`: the adapter answers with its capabilities;
  2. event `initialized`: configuration may come;
  3. `setBreakpoints` (one per file, replacing that file's), `setExceptionBreakpoints`
     (when the capabilities list filters), then `configurationDone`;
  4. `launch` or `attach` may be sent after `initialize`; the adapter answers it after
     `configurationDone`.
- **On a stop:** event `stopped` (a reason and a thread), then the client asks `threads`,
  `stackTrace`, `scopes`, `variables`. References are valid only while stopped.
- **The minimal set for breakpoints, stepping, the stack, variables and watch:**
  - lifecycle: `initialize`, `attach`, `setBreakpoints`, `setExceptionBreakpoints` (ours:
    one filter, "Script errors"), `configurationDone`, `disconnect` (attached: detach and
    let the game go on); events `initialized`, `terminated`;
  - running: `continue`, `next`, `stepIn`, `stepOut`, `pause`; events `stopped` (breakpoint,
    step, pause, exception, entry; `allThreadsStopped`) and `continued`;
  - looking: `threads` (one: "Scripts"), `stackTrace`, `scopes` (Locals, Upvalues,
    Module), `variables` (lazy, by reference), `evaluate` (contexts `watch`, `hover`,
    `repl`);
  - event `output` for `print`, `warn` and logpoints, with source and line.
- **Worth it soon:** `setVariable`, `exceptionInfo` (the error's text in VS Code's
  exception view), `breakpointLocations`, `completions` in the console, `terminate` (Stop
  Play).
- **Capabilities to declare:** configurationDone, conditional, hit-conditional and log
  breakpoints, evaluate for hovers, set variable, exception info, the exception filter.
- **Transport:** VS Code starts an adapter executable over stdio, connects to an adapter
  serving a TCP port (`DebugAdapterServer`), or runs one inline.
  - Attaching to a running game is the TCP case. Stdio fits an adapter VS Code launches
    (it could be a thin proxy to the game's port).
  - A debug type comes from an extension (`contributes.debuggers`). Ours: a small extension
    with the type and an attach configuration returning `DebugAdapterServer(port)`.
    `debugServer` in `launch.json` is the documented shortcut for adapter authors.
- **The endpoint:**
  - the editor, and the player when asked, listen on 127.0.0.1, not 0.0.0.0 as
    luau-debugger does: `evaluate` runs code in the game;
  - a fixed default port; port 0 takes a free one, written to the log and to
    `.ofelia/run/` for the extension;
  - one client at a time;
  - behind a cargo feature that `shipping` leaves out (Defold leaves the server out of
    release engines).

## Stopping while the editor draws on the same thread

### What holds what during a stage

The editor's frame is one call: `run_inner` calls
`player.frame_overlaid(&mut game, Some(&mut overlay))`. Inside it:
1. the swapchain image is acquired, before the ticks;
2. `runtime.frame(…)` runs the ticks. Each tick's closure calls
   `scripts.tick_with(world, dt, physics)` before the game's tick and the physics step;
3. the stage opens the world (`OPEN_WORLD`, a raw pointer) and calls each hook through
   `ScriptVm::call_function` (mlua's `Function::call`: `lua_pcall`);
4. after the ticks, the scene is drawn and the editor's UI with it (the overlay).

During a hook these are alive on the stack: `&mut Player` (all of it), the acquired image,
`&mut Scripts`, `&mut World`, a shared borrow of the editor's game in its `RefCell` (for its
physics), and mlua's call.

### (a) Wait inside the callback and draw the editor from there

- **For:** it stops anywhere, and hooks are called as today.
- **Against:** the editor's frames would run inside `frame_overlaid`, which cannot be
  entered again.
  - `&mut Player` is taken: a frame drawn from the callback needs the window, the
    swapchain, the frame slots and the renderers handed down by the tick's closure as
    separate borrows, and the editor's game freed from its `RefCell` borrow.
  - The swapchain image is already acquired: the first frame drawn while stopped must
    present it, and the outer frame must acquire again when the script goes on.
  - Everything else the outer frame holds is off limits until the script goes on: the world
    (read only, through the stage's pointer), `Scripts` (no reload, no Add Component),
    saves, keep changes. Stop must first make the script fail.
- **Stepping:** the callback returns into the interpreter loop already running, so
  `lua_singlestep` does nothing there. Either the whole VM steps all the time
  (luau-debugger: no native code, a check per instruction) or steps are made of temporary
  breakpoints.
- The time budget's deadline passes while waiting: it has to be stopped and given back.

### (b) Break the script, let the frame go on, resume later (ADR 0107 §6's plan)

- **Hooks are entered as Luau threads:** a thread the VM keeps, `lua_resume` instead of
  `lua_pcall`, `lua_resetthread` after an error. A breakpoint calls `lua_break`, and
  `lua_resume` returns `LUA_BREAK`.
- **The stage waits**, and with it the rest of the tick (the game's tick, the physics step,
  the host's snapshots) and the frame hooks. The editor's frame goes on as when Play is
  paused (E3-2, `set_simulating(false)`): its UI and its viewport stay live.
- **Stepping is exact:** set `lua_singlestep` on the thread and resume. `debugstep` breaks
  again at the next line, or when the depth drops for step out.
- **Errors stop with the whole stack:** after `lua_resume` fails, the thread's frames are
  still there.
- **It cannot break where Luau cannot yield** (above).
  - A continuation on `__namecall` makes `self:method()` breakable.
  - Metamethods written in Luau, `table.sort` comparators, `string.gsub` functions and a
    module's top level while it loads stay unbreakable.
- **The Rust stage must stop halfway and go on later.** Two ways:
  - **(b1) a state machine.** Each loop of the stage keeps its place and returns at a
    break: about ten call sites in `scripts.rs` (`follow`, `call_loaded`, `deliver_later`,
    `timers_due`, `touches`, `call_all` with `on_start`, the systems, `deliver` with its 16
    passes, `despawn_asked`), each with its own place to keep.
  - **(b2) a fiber.** While a session is on, the stage runs on its own stack (a Windows
    fiber). A stop switches back to the editor's fiber; the stage's Rust frames wait as
    they are.
    - The stage changes only in how it reaches the world: it reads `OPEN_WORLD` again after
      each hook, as the views already do, and the player opens the world again before
      resuming, so no `&mut World` stays alive across a stop.
    - `Scripts` moves into the stopped stage, so the editor cannot reach it.
    - Where Luau cannot yield, the stop waits inside the callback instead: the fiber keeps
      that C stack too. Stepping from there is approximate (temporary breakpoints for over
      and out; into behaves as over).
    - Stepping from a yieldable stop: the callback calls `lua_break` after turning stepping
      on, so the next `lua_resume` enters the stepping loop.
    - A panic is caught at the fiber's entry and never crosses a switch.
- **The world while stopped:**
  - reading is safe in both: the stage holds no Rust reference to it (b1 has returned; b2
    reads its pointer again);
  - the views find their component by entity and id at every access (`get_raw`), so no
    pointer dangles after an edit. But a value a script copied into a local is stale, and a
    despawned entity's `self` fails at its next access;
  - v1: the play world is read-only while a script is stopped, as ADR 0107 §6 had for its
    fallback. Value edits can come later.
- **Costs,** only while a session is on: a `lua_resume` per hook instead of `lua_pcall` (to
  measure against today's 0.14 µs for an empty `on_tick`, ADR 0114); with b2, two fiber
  switches per stage.
- **Fibers and Tracy:** a zone opened in the stage stays open across a stop while the
  editor's fiber opens and closes its own on the same thread.
  - Tracy's fiber mode (`TRACY_FIBERS`) sends every zone of the program through one
    serialized queue with a lock (`TracyQueuePrepare` becomes `QueueSerial()`): not for
    us.
  - Instead `engine_profiler` closes the stage's open zones before a switch and opens them
    again after.

### (c) Play's world on another thread or in another process

- **Another thread:** the game's thread could simply block, as the others do. But the editor
  reads and writes the play world directly (the tree, the inspector, gizmos, picking, keep
  changes, undo records, the GPU scene's extraction). Every panel would go through messages
  or locks, undoing ADR 0107's E3 decision.
- **Another process** (ADR 0107 §1's "later", the owner's В): the player is the game and
  the editor a client.
  - The debugger is simplest there (block in the hook), and the editor's panel can be a DAP
    client of the game, as Defold's editor is since 2026-10.
  - The live tree, the inspector and keep changes need a remote protocol (Godot's remote
    tree). Weeks of work for E3's features: worth it for crash isolation, not for the
    debugger alone.

### The standalone player

- No editor to keep alive, so a stop can wait in place (Godot's and Defold's model): a loop
  that pumps the window's messages (input dropped, so Windows does not call it hung),
  answers DAP and sleeps 1 ms on a high-resolution waitable timer.
- Hooks are still entered as Luau threads while a client is attached, for exact stepping.
- Started with `--debug-port` (0: a free port, written out) and `--debug-wait` (no tick
  before `configurationDone`).
- A blocked process stops its network traffic: peers time out after the transport's
  timeout, as in Godot and Defold.

## Costs

- **No session: nothing.** No `LOP_BREAK` exists. `debugbreak` is read only at that
  opcode, `debugstep` only in the stepping loop, `debuginterrupt` only when a coroutine
  breaks, `debugprotectederror` only on an error. Hooks go through `lua_pcall` as today.
- **A breakpoint:** one byte patched in each function having that line, and the opcode copy
  (one byte per instruction) once per function. The function runs interpreted until its
  script reloads or Play restarts: 6.4× slower on ADR 0032's vector loop (20.80 ms against
  3.25 ms). Its callers stay native.
- **A step:** the stepped thread runs the stepping loop, interpreted, with a callback per
  instruction, from the step to the stop.
- **A session with no stop:** a `lua_resume` per hook; with b2 two fiber switches per stage;
  the interrupt reads a pause flag where it reads the clock, every 64 interrupts.
- **Debug level 2:** local and upvalue names stored with the bytecode. Memory, no cost per
  instruction; the bytecode's size is to measure.
- **Optimization level 1** (no inlining, so breakpoints and locals match the text): to
  measure against 2 on ADR 0114's scenes (1,000 doors, 10,000 spinners).
- **CLAUDE.md §5:**
  - nothing allocates while running: one thread per VM reused, the fiber made once,
    breakpoints patched in place;
  - a stop allocates once (its frames, the variables' texts); the editor's frames while
    stopped allocate nothing;
  - a condition is compiled once into a function taking the frame's locals as arguments, so
    a hit allocates nothing (luau-debugger and Defold build a table every time);
  - evaluation and logpoints allocate: they are debugging actions;
  - no lock on the main thread: the DAP thread and the main thread trade messages through
    single-producer queues; blocking sockets on a plain thread, no async runtime;
  - callbacks catch panics.

## What we take

1. **The core in `engine_script`,** over Luau's interface:
   - breakpoints set through each module's chunk (kept per module, applied again after
     every load and reload, before the chunk runs), with moved lines reported;
   - conditions, hit counts, logpoints;
   - the stack, locals, upvalues; our userdata (`self`, views, entities, assets) shown
     through reflection, without calling their metamethods (Defold's rule);
   - evaluation on a thread of its own, under the time budget, with stops off meanwhile;
   - errors stopped with their stack ("pause on errors").
2. **Debuggable compilation** in the editor's Play and in a player started for debugging:
   debug level 2; optimization 1 if the measurement allows; native code stays on.
3. **Hooks entered as Luau threads** while a session is on (raw `lua_resume`, one thread
   reused), and continuations on our C functions that call scripts (`__namecall`).
4. **Stops:** (b2) in the editor: the stage on a fiber, the tick waiting, the editor's
   frame going on, the play world read-only. The player waits in place.
5. **Pause:** a flag read in the interrupt, stopping at the next line; with no script
   running, Play pauses (E3-2) with an empty stack.
6. **DAP** on 127.0.0.1 from a thread of its own, the minimal set above; a small VS Code
   extension for the debug type and attaching.
7. **One core, two front ends** (the UX note): the editor's panel calls the core directly.

Not taken: (a) (the frame cannot be entered again), (c) for now, stepping by
single-stepping all the time (every Play would run interpreted), Tracy's fiber mode.

## Corrections to the UX note (2026-10-17)

- `debugprotectederror` runs only in yieldable contexts, never under the host's
  `lua_pcall`. "Pause on errors" rests on `lua_resume`'s intact stack (or on a `lua_pcall`
  error function).
- `lua_singlestep` takes effect at the next entry into the interpreter, not at once.
- The fallback "the game thread waits while the editor's frames go on" (ADR 0107 §6) needs
  a frame that can be entered again; the fiber gives the same without it.

## Open questions for the team

1. **Fiber (b2) or state machine (b1)** for the stopped stage? The fiber stops everywhere in
   a stage and leaves `scripts.rs` as it is; it is a new piece of the threading model (an
   ADR, CLAUDE.md §3) and needs Tracy's zones closed across it.
2. **A module's top level and `on_reload`** run outside the stages. Skip their breakpoints
   with a note in v1, or run loads on the fiber too?
3. **Native code during a session:** on (game speed, callers' locals hidden), or off for the
   session through the C++ shim (every frame readable, about 6× slower on vector code)?
4. **Edits while stopped:** v1 read-only, value edits later?
5. **The editor's Play always with a session on** (breakpoints work without attaching), at
   the price of a `lua_resume` per hook, once measured?
