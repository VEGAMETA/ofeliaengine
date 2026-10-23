# 0119. The script debugger: a stop suspends the script stage

- Status: **Accepted (team decision, 2026-10-23): every recommendation of the questions at
  the end.**
  - Stops: the stage on a fiber of its own (option 1).
  - Edits: the editor edits the world at a stop; where the stage holds its borrows (a stop
    inside a metamethod or a C library's callback), the world is read-only.
  - VS Code: the extension is kept in the repository and installed from a `.vsix`; on the
    Marketplace and Open VSX at the first public release.
  - Pause on errors: on by default in the editor, once per line per Play.
- Date: 2026-10-23
- Amends: [ADR 0107](0107-editor.md) §6, the debugger's pausing. Builds on
  [ADR 0114](0114-luau-api.md) (the script host) and [ADR 0032](0032-scripting.md).
- Research: `docs/research/script-debugger.md` (how). UX: `docs/research/ux/script-debugger.md`
  (what the developer sees).

## Context

- **ADR 0107 §6 planned the stops** as follows:
  - hooks are entered as resumable Luau threads, and a breakpoint suspends the script
    (`lua_break`): the ticks stop and the editor goes on;
  - where Luau cannot suspend, the game thread waits inside the breakpoint, and the
    editor draws its frames from within it, the world read-only.
- **The research found that the second half cannot be built as written.**
  - The editor's frame is one call, `Player::frame_overlaid`. It holds `&mut Player`, and
    it has acquired the swapchain image before the ticks.
  - A frame drawn from inside a breakpoint would enter it again.
- **What Luau 0.736 and mlua 0.12.1 give** (the research note has the details):
  - `lua_breakpoint` patches one opcode, through a module's chunk into every closure made
    from it, and returns the line it moved to.
  - `lua_break` suspends a thread only where it can yield. That is code a host started
    with `lua_resume`. It cannot yield under a host's `lua_pcall`, in metamethods written
    in Luau, in C library callbacks (`table.sort`, `string.gsub`), or in C functions
    without a continuation (ours: `self:method()` through `__namecall`).
  - mlua's `Function::call` is a `lua_pcall`, and its `Thread::resume` takes `LUA_BREAK`
    for an error. So hooks are entered with `ffi::lua_resume` on threads the engine keeps
    while a session is on.
  - `lua_singlestep` is read when the interpreter is entered. Stepping from a callback is
    `lua_break`, then a resume.
  - `debugprotectederror` never runs under `lua_pcall`. "Pause on errors" reads the stack
    `lua_resume` leaves after an error.
  - Native code stays on. A function with a breakpoint runs interpreted until its script
    reloads. A native frame's locals cannot be read.
- **Every debugger studied blocks the thread that runs the scripts:** luau-debugger, Godot,
  Defold, MobDebug, EmmyLua, local-lua-debugger. Their editors run elsewhere; ours runs on
  that thread.

## Options

1. **The stage on a fiber.** While a debugging session is on, the script stage (a tick's
   and a frame's) runs on a Windows fiber of its own.
   - **How a stop works:** it switches back to the editor's fiber. The stage's Rust and C
     frames wait as they are, and the player leaves the rest of the tick for later.
   - **How it goes on:** Continue switches to the stage again. When the stage ends, the
     rest of that tick runs: the game's `tick` and the physics step.
   - **For:**
     - it stops wherever a breakpoint is, where Luau can yield or not;
     - `scripts.rs` stays as it is, but for reading the world's pointer again after each
       hook;
     - stepping is exact at a stop where Luau can yield.
   - **Against:**
     - fibers are new in the threading model;
     - Tracy's zones open in the stage are closed before a switch and opened again after it
       (`engine_profiler`; Tracy's own fiber mode serializes every zone through one lock);
     - a panic is caught at the fiber's entry;
     - the thread's locals are shared, so the editor runs no stage while one is stopped;
     - at a stop where Luau cannot yield, the stage's Rust frames hold borrows of the
       world, so it is read-only until the function returns.
   - **Cost:** only while a session is on: two switches a stage and a `lua_resume` a hook.
2. **A stage that can stop halfway.** The stage's loops keep their place and return at a
   break, and the player enters the stage again later. No fiber.
   - **For:**
     - no new threading piece;
     - at a stop no Rust frame holds the world, so edits are safe at every stop.
   - **Against:**
     - about ten loops of `scripts.rs` made resumable (following entities, `on_load`,
       messages waiting, timers, contacts, the instances and `on_start`, systems,
       messages' passes, despawns), in the code ADR 0114 measured allocation-free;
     - no stop inside metamethods written in Luau or C library callbacks. A breakpoint there
       logs instead of stopping, and its gutter mark says why.
3. **Not taken:**
   - frames drawn from inside the breakpoint (the frame cannot be entered again);
   - Play on another thread or in another process: every panel would go through messages,
     undoing E3's Play on the scene's world. That is weeks of work, worth it for crash
     isolation later, not for the debugger.

## Decision

- **The core is in `engine_script`, over Luau's interface:**
  - breakpoints set through each module's chunk, kept per module, and set again after every
    load and reload; moved lines are reported;
  - conditions, hit counts, logpoints, and pause points (Rider's: the game pauses after the
    tick);
  - the stack, locals and upvalues; our userdata (`self`, views, entities, assets) shown
    through reflection, their metamethods never called;
  - evaluation on a thread of its own, under the time budget;
  - pause on errors from the stack `lua_resume` leaves.
- **Debuggable compilation** in the editor's Play and in a player started for debugging:
  - debug level 2 (names);
  - optimization level 1 if measured cheap on ADR 0114's scenes;
  - native code stays on.
- **While a session is on,** hooks are entered as Luau threads (one thread, reused), and
  `__namecall` gets a continuation, so `self:method()` can stop.
- **Stops:** option 1, the stage on a fiber of its own (the team's answer 1).
  - At a stop where Luau can yield, the editor edits the world as it does in a pause.
  - At a stop where it cannot (inside a metamethod written in Luau, or a C library's
    callback), the stage's frames hold borrows of the world. The world is read-only there
    until the function returns, and the panel says so (the team's answer 2).
- **VS Code:** DAP on 127.0.0.1 from a thread of its own, with the research note's minimal
  set and a small extension. It is left out of `shipping`.
  - The extension is kept in the repository, and `ofeliaengine new` installs it from a
    `.vsix`.
  - It goes on the Marketplace and Open VSX at the first public release (the team's
    answer 3).
- **Pause on errors** is on by default in the editor, once per line per Play (the team's
  answer 4).
- **Steps:**
  1. **E9-1a**, which does not wait for the answers:
     - the gutter;
     - breakpoints kept in the session and set in the VM;
     - logpoints and pause points, with conditions and hit counts on them;
     - measured: nothing without breakpoints.
  2. **E9-1b:** stops, stepping, the panel, values in the code, pause on errors.
  3. **E9-2:** DAP and VS Code.
  4. **E9-3:** the standalone player (`--debug`).

## Consequences

- A stop pauses the game in the middle of a tick. The panel's header says so ("tick
  1,284, under way"), and Shift+F7 finishes the tick.
- Without breakpoints nothing changes: hooks go through `lua_pcall`, and no opcode is
  patched.
- Fibers enter the threading model (CLAUDE.md §5), and only this way:
  - only the script stage runs on one, and only while a session is on;
  - Tracy's zones are closed before a switch and opened again after it;
  - a panic is caught at the fiber's entry.
- The defaults the UX note recommends are taken:
  - breakpoints live in the editor's session, with no `breakpoint()` in scripts;
  - the standalone player listens only with `--debug`;
  - stepping past a hook's end stops at the next script line that runs;
  - `self.locals` is writable in the debugger's Variables;
  - a network session stays alive while stopped, once co-op runs in the editor;
  - v1 skips breakpoints in a module's top level and in `on_reload`, said at the line.

## Questions for the team

Answered 2026-10-23 (team decision): a) to each, the recommendation.

1. **How a stop suspends the stage:**
   - a) a fiber (option 1): it stops everywhere and leaves `scripts.rs` as it is; a new
     threading piece, with Tracy's zones handled across it;
   - b) a resumable stage (option 2): no new piece, and edits safe at every stop; about
     ten loops rewritten, and no stop inside metamethods or `table.sort` callbacks.

   Recommended: a.
2. **Edits while stopped:**
   - a) the editor edits the world at a stop, but where the stage holds borrows (option 1,
     inside a metamethod), read-only there;
   - b) read-only at every stop in v1.

   Recommended: a.
3. **The VS Code extension:**
   - a) kept in the repository and installed by `ofeliaengine new` from a `.vsix` now, on
     the Marketplace and Open VSX at the first public release;
   - b) on the Marketplace now (a publisher account in the team's name);
   - c) no VS Code debugging in E9.

   Recommended: a.
4. **Pause on errors in the editor:**
   - a) on by default, once per line per Play (Godot's way, what ADR 0107 §6 and ADR 0114 §9
     planned);
   - b) off: the error goes to the Output and the game goes on (Unity's and Roblox's).

   Recommended: a.

## As built (E9-1a-1, 2026-10-23): the core's breakpoints, logpoints and pause points

- **`engine_script::debug`:**
  - `ScriptVm::set_breakpoints(module, …)` keeps a module's breakpoints and places them
    through its chunk (`lua_breakpoint`, the chunk kept in `Module`). A comment's line moves
    to the next line with code, which `breakpoint_states` reports.
  - A module loaded again gets its breakpoints before its new chunk runs.
  - An off breakpoint is kept, not placed.
- **`debugbreak`** (Luau's callback, set at the VM's creation, called only at a patched
  instruction) finds the breakpoints of the frame's module and line. Its body runs under
  `catch_unwind`. For each breakpoint it:
  - tests the condition;
  - counts the hit, then applies the hit rule (`Hits`: every, the n-th, from the n-th,
    every n-th);
  - acts: a logpoint keeps its message, a pause point asks for a pause.
- **Evaluation** compiles a condition or a text once per breakpoint and per frame's names,
  into `function(<locals and upvalues>) return … end`:
  - mlua compiles the bytecode only; it is loaded and called with raw calls on the thread
    that reached the breakpoint, so a coroutine's breakpoint (and E9-1b's hook threads) work
    alike;
  - its environment is the frame's function's, so the module's globals are there;
  - a later local shadows an earlier one and an upvalue, and Luau's internal locals are
    left out;
  - breakpoints reached while it runs are passed over;
  - an error (a condition that does not compile, a text that fails) is kept on the
    breakpoint, and the script goes on.
- **`ScriptSettings::debug_level`**, 2 by default: the names conditions and logpoints read.
  It costs bytes of names, not instructions; the measurement is below.
- **The host** (`Scripts`):
  - `set_breakpoints`, `breakpoint_states`, `take_debug_pause` and `module_of(file)` serve
    the editor;
  - at the end of each stage, logpoints' messages go to the log at their lines as `print`
    writes them (`scripts/door.luau:24: …`), so the Output links them;
  - with no message, nothing is made.
- **Tests:**
  - a logpoint reads a parameter, a local (nil before its assignment), an upvalue and a
    global;
  - a comment's line moves;
  - conditions with every-n-th hits, pause points, and errors kept;
  - breakpoints set before the module loads, kept through a reload, off ones not placed;
  - in the host, a logpoint in a component's `on_tick` reads `near` and `self.lift`, a
    pause point's condition holds, and both are kept through the script's reload;
  - the zero-allocation test: 0 in 600 ticks (no breakpoints).
- **Next (E9-1a-2):** the editor's gutter, keys and options; breakpoints kept in the
  session and given to each Play's host; a logpoint's latest message at its line; a pause
  point pausing Play.
