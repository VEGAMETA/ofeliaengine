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

## As built (E9-1a-2, 2026-10-28): the gutter, the session, Play

- **A bug the first step's tests missed, fixed:** a breakpoint was reached once and never
  again on most lines.
  - Luau's loader turns field reads, field writes and method calls with known names into
    userdata instructions (`GETUDATAKS`, `SETUDATAKS`, `NAMECALLUDATA`). On their slow path
    they write the plain instruction's opcode back over their own (`VM_PATCH_OP`), so the
    breakpoint's `LOP_BREAK` in that byte went too.
  - E9-1a-1's tests broke on arithmetic (`speed * 2`), which has no such instruction. A
    component's `self.Transform:rotate(…)` line has one.
  - Now each hit is noted, and those lines are placed again through their modules' chunks
    (kept as registry references) at the next interrupt (a call, a return, a loop's turn)
    and before the next call into a script. The callback sets the flag as it returns:
    placed again from inside it (a logpoint's text calls and returns), the instruction
    would undo it.
  - A host test now ticks a logpoint in `on_tick` over three instances: 3, 6, 9, 12 hits.
- **The core's additions:**
  - a list given again keeps the states (hits, message, error) of the breakpoints it still
    asks for, since the editor gives a module's whole list when one changes;
  - `reset_breakpoint_states` at each Play;
  - `breakpoints_version`, which the editor compares before it reads the states;
  - a pause point reports its module and line.
- **The player:** a pause point reached in a tick ends the frame's ticks after that one and
  keeps the world still (`Player::take_debug_pause`); the editor pauses Play as F7 does and
  says where.
- **The editor** (`docs/research/ux/script-debugger.md` §1, §2, §8):
  - a breakpoints' column left of the numbers, in the game's scripts (`.luau` under
    `scripts/`);
  - marks by shape: ◆ a logpoint, a pause point a circled ‖, both hollow when off, a bar
    inside for a condition or a hit rule, a warning's dot for a problem; a faint mark where
    the mouse is;
  - after each line, a logpoint's latest message and its hits (`speed = 2  ×312`), or why
    it does nothing (no code from its line on, its condition's error, waiting for a save);
  - the mouse: a click sets or removes, Shift a logpoint, Alt off or on, the middle button
    removes, the right one opens the options;
  - the keys in a script's tab: F9, Shift+F9 (a logpoint), Ctrl+F9 (off or on), Alt+F9
    (options), Ctrl+Shift+F9 (remove all; the palette's "Restore Removed Breakpoints" brings
    them back);
  - the options line under the code: what it does (Log or Pause), the text, the condition,
    the hit rule (`10`, `>= 10`, `% 64`); Tab between the fields, Enter keeps (a press in
    the code too, once they read), Escape drops;
  - **until stops come (E9-1b), F9 and a click make a pause point**; with E9-1b they make a
    breakpoint that stops at the line, and a pause point comes from the options;
  - breakpoints follow their lines through edits (a line goes with its start, as VS
    Code's); two brought to one line keep the one whose text is there;
  - the host gets the lines of the code that runs: a document keeps each line's line in
    the text as last saved, so unsaved edits move the marks only, and a breakpoint on a
    line made since waits for the save; a script closed without saving takes its marks
    back to its file's lines;
  - a comment's or a blank line's breakpoint moves to the line the host placed it on;
  - kept in the session as `[breakpoint]` blocks (lines from 1), brought back at the start;
    given to the host where they changed (once a frame, after the scripts that changed ran
    again) and all again to a new host.
- **The platform:** F10 alone no longer starts Windows' window menu (the frames waited for
  the next key); the step over (E9-1b) and games get it.
- **Measured** (ref-1, release; 1,000 instances of a one-line `on_tick`, medians of 51
  ticks; `measure_breakpoints`):

  | | tick | per instance |
  |---|---:|---:|
  | no breakpoint | 0.28 ms | 0.28 µs |
  | a logpoint turned off | 0.25 ms | 0.25 µs |
  | a logpoint on every 64th hit | 0.86 ms | 0.86 µs |
  | a pause point, its condition false | 1.33 ms | 1.33 µs |
  | a logpoint on every hit | 1.64 ms | 1.64 µs |

  - Nothing without breakpoints: none is placed, the editor gives and reads nothing, and its
    frame allocates nothing (the zero-allocation test).
  - A hit costs the callback, its line placed again, and its condition or text. The
    function runs interpreted, since native code has no breakpoints.
- **Not done:**
  - a logpoint's message allocates (its text, the log's record): the UX note's "into the
    log's ring" is left for later;
  - the options line lies over the lines below it, which VS Code moves down;
  - the "only for" filter of entities waits for the stops' header (E9-1b);
  - the Breakpoints list waits for the panel (E9-1b).
- **Tests:**
  - the code core: lines followed, saved lines kept, unsaved text against its file;
  - the breakpoints: hit rules, following, the host's lines, removing all and back;
  - the session file's `[breakpoint]` blocks;
  - in the editor on the player, a logpoint from the session logs in Play (its message at
    its line), F9 sets a pause point that pauses Play, Alt+F9 gives it a condition that
    lets Play go on, and the session keeps both.

## As built (E9-1b-1, 2026-10-28): stops on the fiber, F5, the stopped line

- **Fibers** (`engine_platform::fiber`): one fiber, made once, runs one job after another;
  `suspend` gives the thread back from inside a job, `resume` goes back into it. The thread
  becomes a fiber the first time and stays one. A job's panic is caught at the fiber's
  entry and handed back. `GetCurrentFiber` is an inline function of `winnt.h`, not an
  export, so it is read from the TEB (`gs:[0x20]`).
- **The core** (`engine_script::debug`):
  - `BreakAction::Stop` describes the frame (`Stopped`: module, line, locals, its
    function's upvalues, the stack), values read as they are, never through metamethods
    (a userdata by its metatable's `__type`, a table with its length, a string cut at 60
    characters), and calls the host's `StopHandler`;
  - the handler waits and answers `Resume::Continue` or `Resume::Abort`; None, where it
    cannot wait, makes the stop a pause point;
  - the call's time budget is held while it waits;
  - `Abort` raises its error once the callback's own frames are done (outside
    `catch_unwind`), the call fails with `ScriptError::Aborted`, and until the host ends it
    (`end_abort`) every later stop answers it and every call fails at once, so the stage
    runs out fast;
  - `ScriptDebugger`, the debugger apart from the VM (it holds the VM weakly), sets and
    reads breakpoints through each module's chunk reference, so the editor uses it while a
    stage holds the VM. A reload that fails puts the old chunk back for it.
- **The player** (`engine_app::script_debug`):
  - with `Player::set_script_debugging(true)` (the editor's Play), the tick's and the
    frame's script stages run on the fiber;
  - a stop suspends it: the frame goes on without the rest of the stage, without the game's
    tick, the physics step and its interpolation, and later frames run no ticks and no
    scene following (`before_ticks`) until `continue_script`;
  - Continue: at the start of a later frame the stage goes on with that frame's world and
    physics world opened anew (`scripts::reopen`); when it ends, the rest of its tick runs
    (the game's tick, the physics step, the host's end of the tick);
  - `end_script_stop` (Play stops, `end_run`, the player's drop) aborts it, no physics
    world open; an aborted call stops and reports no instance;
  - while a stage is held the player hands out no `Scripts` (`scripts()` is None),
    `load_script` and `reload_script` refuse, saves wait, and breakpoints go through
    `Player::script_debugger`;
  - a joined client's stages stay off the fiber (its prediction drives its ticks).
- **No frame of a waiting stage holds a borrow the editor's frames conflict with**, so the
  world can be edited at every stop, not only where Luau could yield (the decision's
  answer 2 asked for read-only where the stage holds borrows):
  - the stage reaches the world and the physics world only through its open pointers, read
    again after every hook (`tick_open`, `frame_open`, `follow`, `despawn_asked`; a step's
    contacts are copied out before they are heard);
  - the host is boxed, and the editor does not touch it while it is held;
  - the views' C functions hold no reference across a call back into Luau (`self:open()`
    calls `Door.open` through raw pointers), and a query's loop checks each entity again
    at each step, so edits at a stop inside a loop are seen, not undefined.
- **Tracy:** zones opened in a fiber's job are noted (`engine_profiler::enter_fiber`,
  `leave_fiber`), closed when it gives the thread back and opened again when it goes on;
  Tracy's own `TRACY_FIBERS` would put every zone of every thread through one lock. On
  demand (`tracy-ondemand`) a zone's end needs its connection, which the API does not give:
  zones there are left as they are.
- **The editor** (the UX note's §1, §5 and §6):
  - F9 and a click make a breakpoint that stops (●, hollow when off); the options' switch
    goes Stop, Log, Pause; a session's `[breakpoint]` block with neither `log` nor `pause`
    stops;
  - at a stop: the script's tab at the line, an amber band and the arrow over its mark,
    the frame's values after the line, the viewport's edge amber, the mouse freed (given
    back when it goes on), the status bar saying where and which keys go on; the game's
    hooks wait; new and saved scripts wait;
  - F5 while playing goes on (a stopped script, else a paused game), in the game's window
    too; F8 ends Play there.
- **Measured** (ref-1, release; 1,000 instances of a one-line `on_tick`, medians of 101
  ticks; `measure_stage_on_fiber`): 0.263-0.267 ms a tick straight, 0.264-0.265 ms on the
  fiber, two runs: the two switches are below the tick's noise. A Tracy zone costs 18.0 ->
  19.2 ns (`benches/zones.rs`, the thread-local read that notes it).
- **Not done (E9-1b-2 on):**
  - stepping (F10, F11, Shift+F11): hooks entered as Luau threads, `lua_singlestep`;
  - the Debugger panel: the stack, the variables, the header (its entity, "tick 1,284, under
    way"), Shift+F7 to finish the tick;
  - values on the lines above the stopped one, hover values, our userdata through
    reflection (`self` shows `Self` now);
  - pause on errors;
  - the game's window keeps its focus at a stop; the editor's window does not take the
    keyboard yet.
- **Tests:**
  - fibers: a job runs, suspends, goes on; a panic comes back; two fibers on one thread;
  - the core: a stop waits on a fiber with its frame (locals, upvalues, the stack), the
    editor's breakpoints changed meanwhile, its budget held past its length, Continue,
    Abort and the calls failing until `end_abort`; values read as they are;
  - the player's stage: held while the world is edited and a breakpoint taken away, then
    gone on with the edits seen; ended at a stop, the stage's other calls failing, no
    instance stopped;
  - the editor on the player: F9 stops Play at the line (the arrow, the values), the world
    still, an edit at the stop kept, Ctrl+F9 and F5 go on, a condition from the options,
    F8 at a stop ends Play with the world put back, the session keeps the breakpoint.

