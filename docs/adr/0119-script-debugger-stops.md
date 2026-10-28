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

## As built (E9-1b-2, 2026-10-28): steps

- **Hooks on a thread of the debugger's** (`ScriptVm::set_debugging`, the editor's Play):
  - a Luau thread made once, kept in the registry; a hook's function and arguments move to
    it, it is resumed (`lua_resume`) until it returns, its results or its error's value
    move back; a failed thread is reset;
  - only while a breakpoint that stops is set or a step is under way: a resume costs
    0.12 µs a hook more than `lua_pcall` (below), and a stop on a hook called the plain way
    cannot be stepped from (`Stopped::steppable`);
  - a call into a script from inside a hook is made the plain way.
- **A step** (`Resume::StepOver`, `StepInto`, `StepOut`):
  - from a stop on the hook thread where Luau can yield, single-stepping goes on and the
    callback breaks out of the interpreter (`lua_break`); the host resumes at once, and
    the interpreter, entered again, runs its stepping loop (Luau reads the flag at entry);
  - the resume runs the breakpoint's BREAK again, which is passed once;
  - `debugstep`, before each instruction, stops where the step is due: over a call at the
    same depth once the line moved; into anything once the line moved or the depth
    changed; out once the depth dropped; in a later hook call at once (a step past a
    hook's end stops at the next script line that runs, the UX note's default);
  - the stepping loop does not step a BREAK: a step reaching a breakpoint's line stops at
    the breakpoint, after its logpoints;
  - Continue at a stop turns stepping off and breaks out again, so the rest runs in the
    plain loop;
  - a coroutine made while stepping copies the flag; it goes on (no steps in coroutines in
    this step).
- **`self:method()`:** `self`'s `__namecall` calls the component's own function from C. With a
  continuation (`lua_pushcclosurek`), Luau can break out across it: a stop in the method
  can be stepped from and out of, the continuation returning the method's results. Where
  no continuation exists (a `table.sort` comparison, a metamethod written in Luau, a C
  library's callback), a stop cannot be stepped from; the editor says so and F5 goes on.
- **Debuggable compilation:** `ScriptSettings::optimization_level`; the editor's player
  (`PlayerSettings::debuggable_scripts`) compiles with level 1, which inlines nothing: at
  level 2 a small local function is inlined into its caller, and a step goes through its
  lines in the caller's frame. Measured below: level 1 costs nothing measurable. Native
  code stays on; in the stepping loop it is never entered.
- **The player:** `step_script(how)`, the stop's count (`script_stops`, a step's next stop is
  a new one at the same line or not).
- **The editor:** F10, F11, Shift+F11 (VS Code's, Visual Studio's, Roblox's), in the game's
  window too and while a code tab has the keys; each stop's status says which keys go on.
- **Measured** (ref-1, release; ADR 0114 §13's scenes, `measure_hooks_and_views`, medians
  of 51 ticks):

  | | level 2 | level 1 | level 1, a stop set |
  |---|---:|---:|---:|
  | an empty `on_tick` | 0.145 µs | 0.143 µs | 0.26 µs |
  | 1,000 doors | 0.591 ms | 0.591 ms | 0.79 ms |
  | 10,000 spinners, a system | 1.243 ms | 1.220 ms | 1.24 ms |
  | 10,000 spinners, a hook each | 2.809 ms | 2.843 ms | 4.38-4.44 ms |

  Debugging with no breakpoint that stops costs nothing more (0.143 µs, the hooks called the
  plain way).
- **Not done:**
  - steps in coroutines;
  - the Debugger panel (the stack, the variables, the header), values on the lines above;
  - pause on errors; the Pause command stopping at the next line.
- **Tests:**
  - the core: over, into, out through a local function (level 1), past a hook's end into the
    next call; debugging off, a stop is not steppable; in `table.sort`'s comparison, not
    steppable, and a step from the line after goes on into the next call;
  - the host: a stop in `self:open()` is steppable, out of it to the line after the call,
    the method's result kept;
  - the editor on the player: F10 twice (the logpoint's line, the hook's end, a local made
    since among the values), F5 to the next tick's stop, then E9-1b-1's flow.

## As built (E9-1b-3, 2026-10-28): the Debugger panel

- **A bottom tab** after Output and Tasks (the layout's), Ctrl+Shift+D (VS Code's Run and
  Debug). It opens by itself at a stop and stays open (Godot's).
- **The header:** Continue, Over, Into and Out with their keys, dim where they do nothing
  (no stop, or a stop that cannot be stepped from); the stop's hook and place
  (`Stopped · on_tick · scripts/spin.luau:3`), or why there are no steps there.
- **Call Stack:** the stop's frames, the stopped one first with the arrow; a click shows that
  frame's variables and its line in its script. A C function's frame reads `engine`.
- **Variables:** the shown frame's LOCALS, then UPVALUES. The core describes every frame of
  a stop with its own (`Frame::locals`, `Frame::upvalues`).
- **Breakpoints:** each with its mark, its place, its line's code (cut at 32 characters),
  what it does in words (`logs …`, `pauses after its tick`, `if …`, `hits …`) and its hits
  since Play; a click on the mark turns it off or on, on the row goes to its line.
- **Its texts** are made again only when the stop, the shown frame, the breakpoints (a
  revision raised at each change) or the host's states (their version) change: a frame
  while it is open makes nothing new.
- **Not done:** the header's entity and tick ("tick 1,284, under way") and Shift+F7; "Only
  Front door"; Copy; "Skip breakpoints"; tables, entities and components opened in the
  Variables; Watch and Evaluate; values changed since the last stop lit; F2 to change a
  value; the list's keys (Space, Delete, Enter); the errors' rows.
- **Tests:** the editor on the player: at the stop the panel shows the header, the frame, its
  locals and both breakpoints with their code; its Over button steps.

## As built (E9-1b-4, 2026-10-28): stops at errors, and a break at the next line

- **Which errors stop:** `ErrorStops { uncaught, caught }` (`ScriptDebugger::set_error_stops`),
  the editor's Breakpoints list's first two rows, "Errors the script does not catch" (on) and
  "Errors caught by pcall" (off), kept in the session (`error_stops = (true, false)`).
- **An error the script does not catch** stops at the innermost script line (`error` and the
  host's functions are C frames, skipped), before the frames unwind:
  - **called the plain way,** a hook runs under the debugger's message handler, which Luau
    calls where the error was raised (`lua_pcall`'s handler sits above the frames that
    raised it). The handled call replaces mlua's `Function::call` while errors stop: 14 ns a
    call more at most (an empty `on_tick` 0.146 -> 0.160 µs; 1,000 doors 0.604 -> 0.621 ms).
    The hook thread is not needed for it: its 0.12 µs a hook is paid only for stops and
    steps, as before;
  - **on the hook thread,** from the frames a failed `lua_resume` leaves until the thread is
    reset; the message is read on the main thread (a dead thread runs no `__tostring`).
- **An error a `pcall` catches** stops in Luau's protected-error callback, the frames still
  there. Luau calls it only where the thread could yield, so hooks go through the hook
  thread while this is on. Ended there, the call ends at its next interrupt (the callback
  may not raise).
- **Once a line until the states are reset** (a new Play); where the host cannot wait, a
  pause after the tick. Continue lets the error take its course (the instance stops, ADR
  0114 §9); Abort ends the call as at a breakpoint; there are no steps from an error.
- **The message:** a string without its place (`attempt to index nil with 'x'`), the host's
  errors (mlua's) as they format themselves, anything else by its type. The time budget's
  error stops inside the loop where the time ran out.
- **Native frames' locals:** Luau's `lua_getlocal` refuses a native frame. A stop at an
  error in native code (every function but a module's top level without loops, Luau's rule)
  shows its line, its message, the stack and the upvalues, but not the frame's locals;
  `Frame::native` marks it (a function with parameters showing no locals), and the panel
  says that a breakpoint in the function shows them. Measured to decide on it: interpreted
  scripts cost 8-33 % more on ADR 0114's scenes (1,000 doors 0.604 -> 0.719 ms; a
  component field's read and write 75 -> 100 ns); the question is the team's.
- **Break at the next line** (`ScriptDebugger::break_next`, the palette's "Break at the
  Next Script Line"): the hook thread single-steps, and the first line a hook runs stops.
- **Fixed: Continue no longer breaks out of the stepping loop.** Breaking out resumed the
  frame where it was, and a native frame's code was entered halfway through a block: an
  access violation (seen with the break above, at a native hook's second line). Continue
  now turns single-stepping off and the stepping loop runs the rest of that hook; Luau's
  native code is entered only where it can resume (a function's start, after a call).
- **Tests:** the core (an error stops once a line on either way hooks are called, ended
  there or let go; the message of `error`; caught errors only when asked; the budget's
  error inside its loop; native frames marked; a break at the next line and a step from
  it); the editor on the player (two instances' error stops once; F5; the break; the row
  turning errors off; the session).

## As built (E9-1b-5, 2026-10-28): values in the code, the Variables opened, Watch and Evaluate

- **Values in the code:** `Frame::defined` (its function's first line) and
  `Frame::params`. The editor reads, for each line of the shown frame's function above its
  line, what the line assigns (`local a, b =`, `a =` and `a +=`, `for i =`, `for k, v in`,
  `self.speed =` and `t.rotation =` as paths; strings, comments and table constructors'
  fields skipped) and shows the values after it, dimmer than the code, after its
  breakpoint's note; the function's line has its parameters. A local whose value changed
  since the last stop in the same function is lit, there and in the Variables.
- **What a value holds:** `Variable::children`, two levels down from a frame's variable,
  within 4,000 values a stop:
  - a table's fields read raw, nothing called: its array first, the rest by name; the first
    100 (`CHILDREN`), then a row for how many more;
  - the host's userdata through its `Describer` (`ScriptDebugger::set_describer`), called
    inside the stop. engine_app's reads through reflection: `self` (its props, its
    entity's other components, `entity`), entities (their `Name`, their components), views
    (their fields as the scene writes them, `engine_scene::write_lines`), assets (their
    paths).
- **The Variables are a tree:** `self` open by default, a click opens or closes a row, the
  paths opened kept through stops.
- **Evaluate:** `ScriptDebugger::evaluate(frame, code, budget)`, the frame's locals and
  upvalues as parameters by name and its module's environment, on a thread of its own (the
  stopped one may be one an error left), under its own budget, breakpoints and error stops
  passing; the value described as a variable. `Player::evaluate_script` opens the player's
  world to scripts for it, as a hook has it (the stage stopped holds no borrow of it).
- **The editor:** the Evaluate line under the Variables (Enter: its answer above it, the
  last 20 kept until Stop; Ctrl+Enter: a watch); the watches first in the Variables,
  evaluated again at every stop and frame, kept in the session (`watches = [...]`), an ×
  takes one away. Play's and the steps' keys (F5, F8, F10, F11, Shift+F11) work while a
  field is typed in, as from VS Code's debug console.
- **Not done:** Globals in the Variables; `self.locals` opened; F2 to change a value; Up
  and Down recalling earlier lines; tables past their first hundred fields; a native
  frame's locals (Luau does not read them, the team's question).
- **Tests:** the core (a table's first hundred fields and how many more, two levels down;
  an evaluation's value, what it holds, its errors and its budget); the editor on the
  player (the parameters on the function's line, `t` and `t.rotation` after theirs, `self`
  open with its prop, components and entity, a component opened by a click, an answer, a
  watch, the session's watches).

## As built (E9-2, 2026-10-28): VS Code over DAP

- **The endpoint** (`engine_editor::dap`), the editor session's: 127.0.0.1, a port the
  system picks, written with a 128-bit token (the system's random generator) to
  `.ofelia/debug/editor.json` and taken away with the session. One client at a time;
  `attach` must give the token, a wrong one is answered and let go.
- **Threads:** one accepts and reads (raw reads into its own buffer, 100 ms at most each:
  Windows wakes no read blocked on a socket another handle shut), one writes and closes the
  connection once what it was given is written; they trade messages with the editor's frame
  through channels. The frame never waits on the socket and makes nothing while no client
  speaks.
- **Requests answered in the frame:** initialize (configurationDone, conditional,
  hit-conditional and log breakpoints, hovers, the two error filters with the editor's
  settings as their defaults), attach (`token`; `play` plays the open scene and the session's
  end stops it), setBreakpoints (the editor's own set for the file, lines through unsaved
  edits), setExceptionBreakpoints, configurationDone, threads (one, "Scripts"), stackTrace,
  scopes (Locals, Upvalues), variables (references valid while stopped, down what a value
  holds), evaluate (in the frame asked, `Player::evaluate_script`), continue, next, stepIn,
  stepOut, pause (the next line a script runs), disconnect.
- **Events:** stopped (breakpoint, step, pause, exception with the error's message),
  continued, output (the scripts' log records: logpoints, `print`), initialized.
- **The extension** (`tools/engine/vscode/ofeliaengine`): the debug type `ofeliaengine`, two
  attach configurations, the endpoint read and the token given, `DebugAdapterServer`.
  `ofeliaengine new` packs it into a `.vsix` (a stored zip with the manifest VS Code reads),
  writes `.vscode/launch.json` and `extensions.json`, and installs it with `code` when it is
  on the path (`--no-extension` does not). Checked by installing the `.vsix` into a throwaway
  extensions folder with VS Code's own command line.
- **Not done:** the editor's breakpoint changes sent to VS Code (`breakpoint` events);
  `setVariable`; `exceptionInfo`; `breakpointLocations`; the console's completions;
  `terminate`; the standalone player's endpoint (E9-3); the extension on the Marketplace
  (at the first public release, the team's answer 3).
- **Tests:** a DAP client speaks to the editor on the player as VS Code does: a wrong token
  refused and let go; initialize, attach, setBreakpoints (a stop and a logpoint),
  setExceptionBreakpoints, configurationDone; Play to the stop; threads, stackTrace, scopes,
  variables (self opened to its prop), evaluate; next (reason step, line 4); the breakpoint
  taken away, continue, the logpoint's message in the console, no stop after; disconnect;
  Attach and Play started and stopped; the endpoint's file gone with the session. The CLI's
  `.vsix`: its CRC-32 and entries.

## As built (E9-3, 2026-10-28): the standalone player

- **Asked on the command line:** `engine_app::run` (what a game's `main!` runs without the
  editor) reads `--debug` and `--wait-for-debugger` into `PlayerSettings::debugger`
  (`DebuggerSettings { wait }`), where the game's settings left it empty; the game's own
  arguments are left to it. Only a development build listens (debug assertions on: `cargo
  run`, the convention `FrameCap::for_build` and saves' text encoding follow); a release or
  shipping build logs that it has no debugger. Listening, the scripts are compiled to be
  debugged (level 1) and debugged on the fiber, as in the editor's Play.
- **One endpoint for both** (`engine_app::dap`, moved from the editor): `Dap::start` takes
  the debuggee, `editor.json` or `player-<pid>.json` in `.ofelia/debug/`, with the title for
  the extension's list. The requests about a stop (threads, stackTrace, scopes, variables,
  evaluate) are answered by one `References::answer`, the editor's and the player's alike;
  `capabilities`, `stopped`, `continued`, `error_stops`, `project_file` and `Asked` (a
  breakpoint VS Code asks for, made the host's with `Hits::parse`, moved from the editor
  into `engine_script`) are shared too.
- **The player's turn** (`player::debugger`), once a frame before a stopped script goes on:
  - a stop's start and end told once each (`stopped` with breakpoint, step, pause or
    exception; `continued`), the window's title saying where ("stopped at
    scripts/door.luau:12", "an error at") and the mouse freed, locked again when it goes on;
  - its requests answered: setBreakpoints by module, through the VM's debugger, answered at
    the lines Luau placed them on; setExceptionBreakpoints; configurationDone (it ends the
    wait); continue, next, stepIn, stepOut; pause (`break_next`);
  - the scripts' log records to the console while a client is attached.
- **Nothing without a client:** the player has no breakpoints of its own and no error stops
  (uncaught errors stop by default once a client says so, as in the editor). When the client
  goes, by its word or not, its breakpoints and error stops go and a stop goes on; a stop
  that comes while none is attached (a pause asked for and reached after the client went)
  goes on at once.
- **`--wait-for-debugger`:** the world is kept still (`simulating` off), the title says it
  waits, until the first configurationDone.
- **The log:** `engine_app::run` installs the engine's logger unless the game installed one
  (stderr, the debugger's output window, `%LOCALAPPDATA%\ofeliaengine\logs\<exe>.log`): a
  game made by `ofeliaengine new` logged nowhere before, and its ring feeds VS Code's
  console.
- **The extension** (0.2.0): `"target": "player"` attaches to the newest player of the
  project whose process runs, or to one picked from a list; `ofeliaengine new` writes
  "Attach to the player" into `launch.json`. Checked by installing the `.vsix` into a
  throwaway extensions folder with VS Code's own command line.
- **Found on the way:** the game template's shipping profile still had `panic = "abort"`
  (ADR 0118 changed only the engine's): it unwinds now.
- **Not done:** Pause with no script running (the editor pauses the ticks; the player waits
  for the next script line); `terminate` (closing the player from VS Code); a launch
  configuration that builds and starts the player; attaching from another machine.
- **Tests:** a DAP client attaches to an offscreen player started as `--wait-for-debugger`
  asks: the world waits until configurationDone; the breakpoint stops the first tick with
  the mouse freed; threads, stackTrace, scopes, variables (`self`), evaluate; next (reason
  step, line 4); the breakpoint taken away, continue, the logpoint's message in the console,
  the mouse locked again; disconnect takes the breakpoints and error stops away; a pause
  left behind is reached and goes on; the endpoint's file goes with the player. The editor's
  DAP test runs on the same client (`engine_app/tests/common/dap_client.rs`).

## As built (E9-4a, 2026-10-28): the stopped entity, the tick, Shift+F7, "Only Front door"

- **The instance a hook runs for** (`engine_script::with_instance`): the host calls every
  instance's hook (start, tick, frame, touches, load, messages, stop) through `call_for`,
  which sets a thread-local to the entity's bits around the call and puts the one before
  back after it. A stop names it (`Stopped::instance`); a system's call has none. Measured
  (A/B in one build, three runs each, medians of 51 ticks): an empty `on_tick` 0.145 ->
  0.151 µs, 10,000 spinners a hook each 2.97 -> 2.94 ms, within the spread of the systems
  that do not take the path (1.24 -> 1.30 ms).
- **A breakpoint limited to instances** (`Breakpoint::only`, the host's ids; empty: every
  one) passes in the hooks of others, and in a system's: checked before its condition, a
  thread-local read. The editor's `Mark::only` keeps `Only::Scene(SceneId)` (written to the
  session as `only = [@id, …]`) or `Only::Spawned(Entity)` (until Play stops, then dropped:
  the breakpoint acts for every instance again); the host is given the entities' bits,
  looked up only for the breakpoints given to it, and an id no instance has when every one
  is gone.
- **The header** names the entity and the tick (counted from Play's start, the ticks that
  simulated only: the runtime also counts the steps a still world skips, which made a stop's
  tick run ahead before this): `Stopped · on_tick · Front door · scripts/door.luau:14 · tick
  1,284, under way`, or `after tick N` in the frame's stage (`Player::script_stop_in_tick`,
  `Player::ticks_run`).
- **"Only Front door"** in the header (when the stop is at a breakpoint of an entity's
  hook) limits that breakpoint to the entity, and reads "Every instance" to take it away
  again; the Breakpoints list says `only Front door`, the mark shows the limit's bar.
- **Shift+F7 at a stop** (the header's Finish tick): `Player::finish_script_tick` goes on
  with every stop passed (`ScriptDebugger::skip_stops`, breakpoints, errors and breaks;
  logpoints still log) until the stage ends, then the rest of its tick, and Play is paused
  after it ("The tick finished: paused").
- **Tests:** a stop names its instance, a limited breakpoint passes the others and a
  system's call, skipped stops pass (engine_script); two doors stop in turn in one tick,
  "Only" one of them passes the other in the next tick, Shift+F7 finishes the tick with no
  stop and pauses, the session keeps the filter by scene id (the editor on the player); the
  session file round trip keeps a scene entity's filter and drops a spawned one's; a stop
  goes on to the next instance's stop in the same stage (engine_app).
- **Not done:** the options bar's "Only for" part (`@name` as the palette finds it, the
  selection), filters on systems (a condition does it: `e.name == "Front door"`), the
  player's DAP endpoint naming the entity.

