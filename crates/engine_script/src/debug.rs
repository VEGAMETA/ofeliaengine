//! The script debugger's core (ADR 0119, ADR 0107 §6): breakpoints set in the VM through
//! each module's chunk, and what they do when a script reaches one.
//!
//! - **Placing:** `lua_breakpoint` patches the first instruction of a line (or of the next
//!   line with code, which it returns) in the module's chunk and every function in it, the
//!   closures made from them included. A module loaded again gets its breakpoints again
//!   before its chunk runs.
//! - **When one is reached** Luau calls [`debugbreak`]. It finds the breakpoints of that
//!   module's line, tests their condition and hit rule, and does what they say:
//!   - a **logpoint** formats its text, Luau's interpolation (`speed = {self.speed}`), and the
//!     host logs it at its line; the script goes on;
//!   - a **pause point** asks the game to pause after its tick (Rider's pausepoint);
//!   - a **stop** describes the frame ([`Stopped`]) and calls the host's [`StopHandler`],
//!     which waits until the developer goes on: the host's stage runs on a fiber, and the
//!     handler gives the thread back to the editor (ADR 0119). The call's time budget is
//!     held meanwhile. [`Resume::Abort`] ends the call with an error raised once the
//!     callback's own frames are done. Where the host cannot wait, a stop is a pause point.
//! - **Steps** (E9-1b-2): while debugging, hooks run on a thread of their own, resumed
//!   rather than called ([`crate::ScriptVm::set_debugging`]). A step from a stop on it turns
//!   Luau's single-stepping on and breaks out of the interpreter (`lua_break`), which the
//!   host resumes at once, now in the stepping loop: [`debugstep`] runs before every
//!   instruction and stops at the step's line. Continue turns it off, and the stepping loop
//!   runs the rest of that hook: breaking out of it would resume the frame there, and Luau's
//!   native code is entered only where it can resume (a function's start, after a call),
//!   not halfway through a block. A stop where Luau cannot yield (under a C function without
//!   a continuation: a sort's comparison, a metamethod written in Luau) cannot be stepped
//!   from ([`Stopped::steppable`]).
//! - **Errors** (E9-1b-4): where the host asks ([`ErrorStops`]), an error stops the script
//!   at its line before the frames that raised it unwind, once a line until the host resets
//!   the breakpoints' states (a new Play):
//!   - one the script does not catch, in the message handler of the hook's call
//!     ([`error_handler`]), or, on the hook thread, from the frames a failed `lua_resume`
//!     leaves until the thread is reset;
//!   - one a `pcall` catches, in Luau's protected-error callback ([`protected_error`]).
//!
//!   Continue lets the error take its course; there are no steps from it.
//! - **Break at the next line** ([`ScriptDebugger::break_next`]): the hook thread
//!   single-steps, and the first line a hook runs stops.
//! - **Evaluation:** a condition or a text is compiled once into a function whose
//!   parameters are the frame's locals and its function's upvalues, by name, with the
//!   module's globals as its environment. Each hit passes the values. Breakpoints reached
//!   while one evaluates are passed over.
//! - **Cost:** nothing without breakpoints: no instruction is patched, so Luau never calls
//!   back. A function with one runs interpreted until its module loads again (Luau's native
//!   code has no breakpoints).

use mlua::Lua;
use mlua::ffi;
use std::cell::Cell;
use std::collections::HashMap;
use std::ffi::{CStr, c_int};

thread_local! {
    /// Set while a breakpoint's condition or text runs: breakpoints it reaches pass.
    static EVALUATING: Cell<bool> = const { Cell::new(false) };
    /// A breakpoint was reached since the last [`rearm`]. Luau's loader turns field reads,
    /// writes and method calls with known names into userdata instructions whose slow
    /// path writes the plain instruction's opcode over its own, the breakpoint's with it:
    /// the breakpoints reached are placed again at the next interrupt (a call, a return, a
    /// loop's turn) and before the next call into a script.
    static REARM: Cell<bool> = const { Cell::new(false) };
    /// A stop was answered [`Resume::Abort`], until the host ends it
    /// ([`ScriptDebugger::end_abort`]): later stops answer it too, without the handler, and
    /// calls into scripts fail at once, so the host's stage runs out fast.
    static ABORTING: Cell<bool> = const { Cell::new(false) };
    /// The debugger broke out of the interpreter at a breakpoint (to enter or leave the
    /// stepping loop): the resume runs its BREAK again, which is passed once.
    static PASS_BREAK: Cell<bool> = const { Cell::new(false) };
    /// Calls made on the hook thread: a step past its hook's end stops in the next one.
    static HOOK_CALLS: Cell<u64> = const { Cell::new(0) };
    /// Hooks go through the hook thread: a breakpoint that stops is set, a step is under
    /// way, or a break at the next line is asked. Without, they are called the plain way, a
    /// stop there not steppable.
    static THREAD_WANTED: Cell<bool> = const { Cell::new(false) };
    /// Which errors stop ([`ErrorStops`]), as the calls read it before the debugger's
    /// state: hooks are called with the message handler while errors not caught stop.
    static ERRORS: Cell<ErrorStops> = const {
        Cell::new(ErrorStops {
            uncaught: false,
            caught: false,
        })
    };
}

/// Whether errors the scripts do not catch stop ([`ErrorStops::uncaught`]): hooks are
/// called with the debugger's message handler ([`error_handler`]).
pub(crate) fn uncaught_wanted() -> bool {
    ERRORS.get().uncaught
}

/// Whether hooks go through the hook thread now ([`THREAD_WANTED`]).
pub(crate) fn thread_wanted() -> bool {
    THREAD_WANTED.get()
}

/// A hook begins on the hook thread ([`HOOK_CALLS`]).
pub(crate) fn hook_called() {
    HOOK_CALLS.set(HOOK_CALLS.get().wrapping_add(1));
}

/// Whether calls into scripts fail at once ([`ABORTING`]).
pub(crate) fn aborting() -> bool {
    ABORTING.get()
}

/// Whether breakpoints reached wait to be placed again ([`rearm`]).
pub(crate) fn rearm_pending() -> bool {
    REARM.get()
}

/// Places the breakpoints reached since the last call again, through their modules'
/// chunks: the instructions they patched may have patched themselves back
/// ([`REARM`]). Their lines are placed as they were, so nothing else changes.
///
/// # Safety
///
/// `state` is a thread of the VM, outside any other call into Luau's API.
pub(crate) unsafe fn rearm(state: *mut ffi::lua_State) {
    REARM.set(false);
    // SAFETY: as the caller says; each chunk is pushed, its breakpoints placed through it,
    // and popped.
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
        let lua = Lua::get_or_init_from_ptr(state);
        let mut wanted = {
            let Some(mut dbg) = lua.app_data_mut::<Debugger>() else {
                return;
            };
            let reached = std::mem::take(&mut dbg.reached);
            let mut wanted = Vec::with_capacity(reached.len());
            for (module, line) in reached {
                let placed = dbg.modules.get(&module).is_some_and(|slots| {
                    slots
                        .iter()
                        .any(|s| s.asked.enabled && s.state.placed == Some(line))
                });
                if let (true, Some(&chunk)) = (placed, dbg.chunks.get(&module)) {
                    wanted.push((chunk, line));
                }
            }
            wanted
        };
        wanted.dedup();
        if ffi::lua_checkstack(state, 1) == 0 {
            return;
        }
        for (chunk, line) in wanted {
            ffi::lua_rawgeti_(state, ffi::LUA_REGISTRYINDEX, chunk);
            if let Ok(line) = c_int::try_from(line) {
                ffi::lua_breakpoint(state, -1, line, 1);
            }
            ffi::lua_settop(state, ffi::lua_gettop(state) - 1);
        }
    }));
}

/// What a breakpoint does when a script reaches it and its condition and hits allow.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BreakAction {
    /// Logs its text, `{expressions}` replaced by their values; the script goes on.
    Log(String),
    /// Asks the game to pause after the tick it is reached in.
    Pause,
    /// Stops the script at its line until the developer goes on ([`StopHandler`]); a pause
    /// point where the host cannot wait.
    Stop,
}

/// Which of a breakpoint's hits (counted when its condition holds) it acts on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Hits {
    /// Every one.
    #[default]
    Every,
    /// The n-th only.
    Nth(u64),
    /// The n-th and those after it.
    From(u64),
    /// Every n-th.
    EveryNth(u64),
}

impl Hits {
    /// Whether hit `n` (from 1) is acted on.
    fn admits(self, n: u64) -> bool {
        match self {
            Self::Every => true,
            Self::Nth(k) => n == k,
            Self::From(k) => n >= k,
            Self::EveryNth(k) => k > 0 && n.is_multiple_of(k),
        }
    }
}

/// A breakpoint as asked for, on a module's line (from 1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Breakpoint {
    /// The line asked for.
    pub line: u32,
    /// What it does.
    pub action: BreakAction,
    /// A Luau expression in the line's scope; acted on only when it is true.
    pub condition: Option<String>,
    /// Which hits it acts on.
    pub hits: Hits,
    /// Off: kept, but not placed.
    pub enabled: bool,
}

/// How a breakpoint is doing: where Luau placed it and what it did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BreakState {
    /// The line it was placed on (the next line with code); none until its module is
    /// loaded, or when no line from it on has code.
    pub placed: Option<u32>,
    /// Its hits (its condition holding) since it was set.
    pub hits: u64,
    /// Its latest message (a logpoint's).
    pub last: Option<String>,
    /// Its condition's or text's latest error.
    pub error: Option<String>,
}

/// A logpoint's message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Logged {
    /// The module.
    pub module: String,
    /// The line it was placed on.
    pub line: u32,
    /// The text, its expressions replaced.
    pub text: String,
}

/// Where a script stopped and what its frame holds, as text: values are read as they
/// are, never through their metamethods.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stopped {
    /// The module.
    pub module: String,
    /// The line, where its breakpoint was placed.
    pub line: u32,
    /// The frame's locals in the order they were made: a name made again comes again.
    pub locals: Vec<Variable>,
    /// Its function's upvalues.
    pub upvalues: Vec<Variable>,
    /// The frames from the stopped one out to the host's call.
    pub stack: Vec<Frame>,
    /// Whether the script can be stepped from here: it runs on the hook thread (the host
    /// debugs), and no C function without a continuation lies between (a sort's
    /// comparison, a metamethod written in Luau). Never at an error.
    pub steppable: bool,
    /// The error it stopped at ([`ErrorStops`]); none at a breakpoint, a step or a break.
    pub error: Option<ErrorStop>,
}

/// The error a script stopped at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ErrorStop {
    /// Its message, without its place (`attempt to index nil with 'speed'`).
    pub message: String,
    /// A `pcall` in the script catches it: the script goes on after it.
    pub caught: bool,
}

/// Which errors stop a script while the host can wait (ADR 0119; the UX note's §9), each a
/// row of the editor's Breakpoints list. A stop at an error comes once a line until the
/// breakpoints' states are reset (a new Play).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ErrorStops {
    /// Errors the script does not catch: the hook fails with them.
    pub uncaught: bool,
    /// Errors a `pcall` in the script catches. Luau reports them only where the thread
    /// could yield: hooks run on the hook thread while it is on (the host debugs), and one
    /// caught under a C function without a continuation (a sort's comparison) does not
    /// stop.
    pub caught: bool,
}

/// A named value, as text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Variable {
    /// Its name.
    pub name: String,
    /// Its value: `3`, `"open"`, `table #4`, `(1, 2, 3)`, a userdata's `__type`.
    pub value: String,
}

/// A frame of a stop's stack.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    /// Its module, or `[C]` for a function of the host or a library.
    pub source: String,
    /// The line it is at; none in a C function.
    pub line: Option<u32>,
    /// Its function's name, if it has one.
    pub function: Option<String>,
    /// Its locals there, in the order they were made (none in a C function).
    pub locals: Vec<Variable>,
    /// Its function's upvalues.
    pub upvalues: Vec<Variable>,
    /// Its function runs as Luau's native code, whose frames' locals Luau does not read
    /// (ADR 0119; one with a breakpoint runs interpreted): told by a function with
    /// parameters showing no locals.
    pub native: bool,
}

/// How a stopped script goes on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Resume {
    /// From where it stopped.
    #[default]
    Continue,
    /// To the next line that runs, into a function the line calls (F11).
    StepInto,
    /// To the next line of this function, or of the one it returns to (F10).
    StepOver,
    /// To the line this function returns to (Shift+F11).
    StepOut,
    /// The call into the script ends with [`crate::ScriptError::Aborted`] (Play stops). A
    /// `pcall` in the script catches it as any error.
    Abort,
}

/// The host's side of a stop: given where the script stopped, it waits until the
/// developer says how to go on (ADR 0119: it suspends the fiber the host's stage runs on,
/// and the editor's frames go on meanwhile). None when it cannot wait: the breakpoint
/// pauses the game after its tick instead, as a pause point.
pub type StopHandler = Box<dyn FnMut(&Stopped) -> Option<Resume>>;

/// One breakpoint of a module.
struct Slot {
    asked: Breakpoint,
    state: BreakState,
}

/// What a condition or a text is compiled as, by breakpoint and by the frame's names.
#[derive(Hash, PartialEq, Eq)]
struct CodeKey {
    module: String,
    slot: usize,
    condition: bool,
    names: String,
}

/// The debugger's state, the VM's app data.
#[derive(Default)]
pub(crate) struct Debugger {
    /// Each module's breakpoints.
    modules: HashMap<String, Vec<Slot>>,
    /// Compiled conditions and texts, as registry references.
    code: HashMap<CodeKey, c_int>,
    /// References no longer used, given back at the next breakpoint.
    dropped: Vec<c_int>,
    logged: Vec<Logged>,
    /// The module and line of the pause point that asked for a pause.
    pause: Option<(String, u32)>,
    /// Raised whenever a state changes: what the editor compares before it reads them.
    version: u64,
    /// Each module's chunk, as a registry reference: what [`rearm`] places breakpoints
    /// through again.
    chunks: HashMap<String, c_int>,
    /// The modules' lines reached since the last [`rearm`].
    reached: Vec<(String, u32)>,
    /// What a stop calls; taken out while it runs.
    handler: Option<StopHandler>,
    /// The thread hooks run on while debugging: a stop there can be stepped from.
    hook_thread: Option<core::ptr::NonNull<ffi::lua_State>>,
    /// Single-stepping is on for the hook thread.
    single: bool,
    /// The step under way, if one is.
    step: Option<Step>,
    /// A break at the next line a hook runs is asked ([`ScriptDebugger::break_next`]).
    break_next: bool,
    /// Which errors stop.
    errors: ErrorStops,
    /// The modules' lines whose errors stopped since the states were reset: they stop
    /// once.
    errors_seen: Vec<(String, u32)>,
}

/// A step asked for at a stop: how far it goes and where it began.
#[derive(Clone, Copy)]
struct Step {
    how: Resume,
    /// The stack's depth there.
    depth: c_int,
    /// The frame's source (a pointer Luau keeps) and line.
    source: *const core::ffi::c_char,
    line: c_int,
    /// The hook call it began in ([`HOOK_CALLS`]).
    call: u64,
}

impl Debugger {
    /// Module `module`'s breakpoints from now on, none placed yet; the old ones' lines
    /// returned to be taken out. One asked for as before keeps its hits, its message and
    /// its error: the editor gives a module's whole list when one of them changes.
    pub(crate) fn set(&mut self, module: &str, asked: Vec<Breakpoint>) -> Vec<u32> {
        let mut old = self.modules.remove(module).unwrap_or_default();
        let lines = old.iter().filter_map(|s| s.state.placed).collect();
        let slots = asked
            .into_iter()
            .map(|asked| {
                let state = old
                    .iter()
                    .position(|s| s.asked == asked)
                    .map(|k| old.swap_remove(k).state)
                    .map(|s| BreakState { placed: None, ..s })
                    .unwrap_or_default();
                Slot { asked, state }
            })
            .collect();
        self.modules.insert(module.to_owned(), slots);
        self.forget_code(module);
        self.version += 1;
        self.want_thread();
        lines
    }

    /// Hooks go through the hook thread where a stop could be stepped from or a break is
    /// asked ([`THREAD_WANTED`]).
    fn want_thread(&self) {
        let stops = self
            .modules
            .values()
            .flatten()
            .any(|s| s.asked.enabled && s.asked.action == BreakAction::Stop);
        // Luau reports an error a `pcall` caught only where the thread could yield.
        let caught = self.errors.caught;
        THREAD_WANTED.set(
            self.hook_thread.is_some()
                && (stops || self.step.is_some() || self.break_next || caught),
        );
    }

    /// Every breakpoint's hits, message and error back to none (a new Play), where they
    /// are placed kept.
    pub(crate) fn reset(&mut self) {
        for slot in self.modules.values_mut().flatten() {
            slot.state = BreakState {
                placed: slot.state.placed,
                ..BreakState::default()
            };
        }
        self.logged.clear();
        self.pause = None;
        self.step = None;
        self.break_next = false;
        self.errors_seen.clear();
        if self.single
            && let Some(thread) = self.hook_thread
        {
            // SAFETY: the hook thread, not running between two Plays.
            unsafe { ffi::lua_singlestep(thread.as_ptr(), 0) };
            self.single = false;
        }
        self.want_thread();
        self.version += 1;
    }

    /// Raised whenever a breakpoint's state changes.
    pub(crate) fn version(&self) -> u64 {
        self.version
    }

    /// Module `module`'s compiled code goes (its breakpoints changed, or its chunk).
    fn forget_code(&mut self, module: &str) {
        let dropped = &mut self.dropped;
        self.code.retain(|k, &mut id| {
            let keep = k.module != module;
            if !keep {
                dropped.push(id);
            }
            keep
        });
    }

    /// The enabled lines to place in module `module`, by slot.
    pub(crate) fn wanted(&self, module: &str) -> Vec<(usize, u32)> {
        self.modules.get(module).map_or_else(Vec::new, |slots| {
            slots
                .iter()
                .enumerate()
                .filter(|(_, s)| s.asked.enabled)
                .map(|(k, s)| (k, s.asked.line))
                .collect()
        })
    }

    /// Slot `slot` of module `module` was placed at `placed`.
    pub(crate) fn placed(&mut self, module: &str, slot: usize, placed: Option<u32>) {
        if let Some(s) = self.modules.get_mut(module).and_then(|m| m.get_mut(slot)) {
            s.state.placed = placed;
        }
        // A new chunk: its code is compiled again for it.
        self.forget_code(module);
        self.version += 1;
    }

    /// Module `module`'s breakpoints' states, in the order they were set.
    pub(crate) fn states(&self, module: &str) -> Vec<BreakState> {
        self.modules
            .get(module)
            .map_or_else(Vec::new, |m| m.iter().map(|s| s.state.clone()).collect())
    }

    pub(crate) fn take_logged(&mut self) -> Vec<Logged> {
        std::mem::take(&mut self.logged)
    }

    pub(crate) fn take_pause(&mut self) -> Option<(String, u32)> {
        self.pause.take()
    }

    /// Whether a pause point asked for a pause that is not taken yet.
    pub(crate) fn pause_asked(&self) -> bool {
        self.pause.is_some()
    }

    /// Module `module`'s chunk from now on, as a registry reference; the one it replaces,
    /// to be let go of.
    pub(crate) fn set_chunk(&mut self, module: &str, chunk: c_int) -> Option<c_int> {
        self.chunks.insert(module.to_owned(), chunk)
    }
}

/// The VM's debugger apart from the VM (ADR 0119): what the editor reads and sets while a
/// script is stopped, when the host's stage holds the VM halfway through a call. It holds
/// the VM weakly: with the VM gone, it does nothing.
#[derive(Clone)]
pub struct ScriptDebugger {
    lua: mlua::WeakLua,
}

impl ScriptDebugger {
    pub(crate) fn new(lua: &Lua) -> Self {
        Self { lua: lua.weak() }
    }

    fn with<R>(&self, f: impl FnOnce(&mut Debugger) -> R) -> Option<R> {
        let lua = self.lua.try_upgrade()?;
        let mut dbg = lua.app_data_mut::<Debugger>()?;
        Some(f(&mut dbg))
    }

    /// Module `module`'s breakpoints from now on, replacing its others; placed at once
    /// when it is loaded, else when it loads. Kept through its reloads.
    pub fn set_breakpoints(&self, module: &str, breakpoints: Vec<Breakpoint>) {
        let Some(lua) = self.lua.try_upgrade() else {
            return;
        };
        let Some((old, chunk)) = lua
            .app_data_mut::<Debugger>()
            .map(|mut d| (d.set(module, breakpoints), d.chunks.get(module).copied()))
        else {
            return;
        };
        if let Some(chunk) = chunk {
            for line in old {
                breakpoint(&lua, chunk, line, false);
            }
            place(&lua, module, chunk);
        }
    }

    /// How module `module`'s breakpoints are doing, in the order they were set.
    pub fn breakpoint_states(&self, module: &str) -> Vec<BreakState> {
        self.with(|d| d.states(module)).unwrap_or_default()
    }

    /// The logpoints' messages since the last call.
    pub fn take_logged(&self) -> Vec<Logged> {
        self.with(Debugger::take_logged).unwrap_or_default()
    }

    /// The module and line of the pause point reached since the last call, if one was.
    pub fn take_pause(&self) -> Option<(String, u32)> {
        self.with(Debugger::take_pause).flatten()
    }

    /// Whether a pause point was reached and its pause not taken yet.
    pub fn pause_asked(&self) -> bool {
        self.with(|d| d.pause_asked()).unwrap_or(false)
    }

    /// Every breakpoint's hits, latest message and error back to none; where they are
    /// placed is kept.
    pub fn reset_breakpoint_states(&self) {
        self.with(Debugger::reset);
    }

    /// Raised whenever a breakpoint's state changes.
    pub fn breakpoints_version(&self) -> u64 {
        self.with(|d| d.version()).unwrap_or(0)
    }

    /// Whether module `module` is loaded: a breakpoint it did not place has no code from
    /// its line on.
    pub fn is_loaded(&self, module: &str) -> bool {
        self.with(|d| d.chunks.contains_key(module))
            .unwrap_or(false)
    }

    /// What a stop calls from now on; without one, stops pause the game after their tick.
    pub fn set_stop_handler(&self, handler: Option<StopHandler>) {
        self.with(|d| d.handler = handler);
    }

    /// Which errors stop a script from now on: a stop at the error's line, once a line
    /// until [`Self::reset_breakpoint_states`].
    pub fn set_error_stops(&self, stops: ErrorStops) {
        ERRORS.set(stops);
        self.with(|d| {
            d.errors = stops;
            d.want_thread();
        });
    }

    /// Which errors stop a script.
    pub fn error_stops(&self) -> ErrorStops {
        self.with(|d| d.errors).unwrap_or_default()
    }

    /// The first line a hook runs from now on stops ("Break at the Next Script Line"): the
    /// hook thread single-steps until it does. False where hooks have no thread of their
    /// own (the host does not debug, [`crate::ScriptVm::set_debugging`]).
    pub fn break_next(&self) -> bool {
        self.with(|d| {
            let Some(thread) = d.hook_thread else {
                return false;
            };
            d.break_next = true;
            if !d.single {
                // SAFETY: the hook thread, not running between hooks: the flag is read when
                // the next hook enters the interpreter.
                unsafe { ffi::lua_singlestep(thread.as_ptr(), 1) };
                d.single = true;
            }
            d.want_thread();
            true
        })
        .unwrap_or(false)
    }

    /// Whether a break at the next line waits for a hook to run.
    pub fn break_pending(&self) -> bool {
        self.with(|d| d.break_next).unwrap_or(false)
    }

    /// Calls into scripts run again after a stop answered [`Resume::Abort`]: the host's
    /// stage ran out. A step under way ends with it.
    pub fn end_abort(&self) {
        ABORTING.set(false);
        PASS_BREAK.set(false);
        self.with(Debugger::reset_step);
    }
}

impl Debugger {
    /// The hook thread from now on (none: hooks are called the plain way).
    pub(crate) fn set_hook_thread(&mut self, thread: Option<core::ptr::NonNull<ffi::lua_State>>) {
        self.reset_step();
        self.hook_thread = thread;
        self.want_thread();
    }

    /// No step under way and no break asked, single-stepping off.
    fn reset_step(&mut self) {
        self.step = None;
        self.break_next = false;
        if self.single
            && let Some(thread) = self.hook_thread
        {
            // SAFETY: the hook thread; the flag is read at the interpreter's next entry.
            unsafe { ffi::lua_singlestep(thread.as_ptr(), 0) };
        }
        self.single = false;
        self.want_thread();
    }
}

/// Places module `module`'s enabled breakpoints through its chunk, the registry reference
/// `chunk`.
pub(crate) fn place(lua: &Lua, module: &str, chunk: c_int) {
    let wanted = lua
        .app_data_ref::<Debugger>()
        .map(|d| d.wanted(module))
        .unwrap_or_default();
    for (slot, line) in wanted {
        let placed = breakpoint(lua, chunk, line, true);
        if let Some(mut d) = lua.app_data_mut::<Debugger>() {
            d.placed(module, slot, placed);
        }
    }
}

/// Sets (or clears) a breakpoint on `line` of the chunk kept as registry reference
/// `chunk` and the functions in it: the line Luau placed it on (the next one with code),
/// none when no line from there has code.
fn breakpoint(lua: &Lua, chunk: c_int, line: u32, enabled: bool) -> Option<u32> {
    let line = c_int::try_from(line).ok()?;
    // SAFETY: the chunk is pushed from the registry; `lua_breakpoint` reads its function
    // and patches instructions in place, and the placed line is pushed back as the result.
    let placed = unsafe {
        lua.exec_raw::<c_int>((), |state| {
            ffi::lua_rawgeti_(state, ffi::LUA_REGISTRYINDEX, chunk);
            let placed = ffi::lua_breakpoint(state, -1, line, c_int::from(enabled));
            // What stays on the stack is the result.
            ffi::lua_settop(state, 0);
            ffi::lua_pushinteger(state, placed as _);
        })
    }
    .ok()?;
    u32::try_from(placed).ok().filter(|&l| l > 0)
}

/// What a reached breakpoint asks, copied out of the state before any Luau runs.
struct Ask {
    slot: usize,
    condition: Option<String>,
    action: BreakAction,
}

/// Luau's breakpoint callback (see the module's documentation).
///
/// # Safety
///
/// Luau calls it with a running thread stopped at a patched instruction.
pub(crate) unsafe extern "C-unwind" fn debugbreak(
    state: *mut ffi::lua_State,
    _: *mut ffi::lua_Debug,
) {
    // Its instruction may write its own opcode back over the breakpoint as it goes on,
    // once this returns: placed again at the next interrupt (`REARM`), not at one of the
    // evaluations' interrupts below.
    if PASS_BREAK.replace(false) {
        REARM.set(true);
        return;
    }
    let evaluating = EVALUATING.get();
    // A panic never crosses Luau's frames: the breakpoint is passed over then.
    // SAFETY: as the caller says.
    let then = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
        reached(state, evaluating)
    }))
    .unwrap_or(Then::Go);
    REARM.set(true);
    if then == Then::Break {
        PASS_BREAK.set(true);
    }
    // SAFETY: as the caller says.
    unsafe { then.apply(state) };
}

/// Luau's step callback, before each instruction of a single-stepping thread: the step
/// under way stops where it reaches its line.
///
/// # Safety
///
/// Luau calls it with a running thread in its stepping loop.
pub(crate) unsafe extern "C-unwind" fn debugstep(
    state: *mut ffi::lua_State,
    _: *mut ffi::lua_Debug,
) {
    if EVALUATING.get() {
        return;
    }
    // SAFETY: as the caller says.
    let then = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe { stepped(state) }))
        .unwrap_or(Then::Go);
    // SAFETY: as the caller says.
    unsafe { then.apply(state) };
}

/// What a callback does once its own frames are done.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Then {
    /// The script goes on.
    Go,
    /// It breaks out of the interpreter, which the host resumes at once: to enter or to
    /// leave the stepping loop.
    Break,
    /// The call ends with [`crate::ScriptError::Aborted`].
    Abort,
}

impl Then {
    /// # Safety
    ///
    /// `state` is the thread in Luau's callback; nothing with a destructor is alive in the
    /// caller's frame (an abort raises, a break may not be legal elsewhere but where it was
    /// checked).
    unsafe fn apply(self, state: *mut ffi::lua_State) {
        // SAFETY: as the caller says.
        unsafe {
            match self {
                Self::Go => {}
                Self::Break => {
                    ffi::lua_break(state);
                }
                Self::Abort => {
                    ffi::lua_pushlstring_(
                        state,
                        crate::ABORT_MARKER.as_ptr().cast(),
                        crate::ABORT_MARKER.len(),
                    );
                    ffi::lua_error(state);
                }
            }
        }
    }
}

/// [`debugstep`]'s body: whether the step under way has reached its line, and if so the
/// stop there.
///
/// # Safety
///
/// As [`debugstep`]'s.
unsafe fn stepped(state: *mut ffi::lua_State) -> Then {
    // SAFETY: as the caller says; `ar` is filled for the running frame, its source a C
    // string Luau keeps.
    unsafe {
        let lua = Lua::get_or_init_from_ptr(state);
        if !step_due(state, lua) {
            return Then::Go;
        }
        let mut ar: ffi::lua_Debug = core::mem::zeroed();
        ffi::lua_getinfo(state, 0, c"sl".as_ptr(), &mut ar);
        let source = CStr::from_ptr(ar.source).to_string_lossy();
        let (Some(module), Ok(line)) = (source.strip_prefix('='), u32::try_from(ar.currentline))
        else {
            return Then::Go;
        };
        stop_at(state, lua, module, line)
    }
}

/// Whether the step under way stops where `state` is now: over a call at the same depth
/// once the line moved, into anything once it moved or the depth changed, out once the
/// depth dropped; in a later hook call, at once. A break asked stops wherever it is.
///
/// # Safety
///
/// `state` is a running thread inside one of Luau's debug callbacks.
unsafe fn step_due(state: *mut ffi::lua_State, lua: &Lua) -> bool {
    let (step, ours, next) = match lua.app_data_ref::<Debugger>() {
        Some(d) => (
            d.step,
            d.hook_thread.map(core::ptr::NonNull::as_ptr) == Some(state),
            d.break_next,
        ),
        None => return false,
    };
    if ours && next {
        return true;
    }
    // A coroutine made while stepping copies the flag: it goes on.
    let (Some(step), true) = (step, ours) else {
        return false;
    };
    // SAFETY: as the caller says.
    unsafe {
        let depth = ffi::lua_stackdepth(state);
        let mut ar: ffi::lua_Debug = core::mem::zeroed();
        if ffi::lua_getinfo(state, 0, c"sl".as_ptr(), &mut ar) == 0 || ar.source.is_null() {
            return false;
        }
        let later = HOOK_CALLS.get() != step.call;
        let moved = ar.source != step.source || ar.currentline != step.line;
        later
            || match step.how {
                Resume::StepInto => moved || depth != step.depth,
                Resume::StepOver => depth < step.depth || (depth == step.depth && moved),
                Resume::StepOut => depth < step.depth,
                Resume::Continue | Resume::Abort => false,
            }
    }
}

/// [`debugbreak`]'s body: the line reached noted for [`rearm`], then, unless one of
/// the debugger's own evaluations reached it (`evaluating`), what its breakpoints do.
///
/// # Safety
///
/// As [`debugbreak`]'s.
unsafe fn reached(state: *mut ffi::lua_State, evaluating: bool) -> Then {
    // SAFETY: as the caller says; `ar` is filled by `lua_getinfo` for the running frame,
    // its source a C string Luau keeps.
    unsafe {
        let mut ar: ffi::lua_Debug = core::mem::zeroed();
        if ffi::lua_getinfo(state, 0, c"sl".as_ptr(), &mut ar) == 0 || ar.source.is_null() {
            return Then::Go;
        }
        let source = CStr::from_ptr(ar.source).to_string_lossy();
        let Some(module) = source.strip_prefix('=') else {
            return Then::Go;
        };
        let Ok(line) = u32::try_from(ar.currentline) else {
            return Then::Go;
        };
        let lua = Lua::get_or_init_from_ptr(state);
        let asks: Vec<Ask> = {
            let Some(mut dbg) = lua.app_data_mut::<Debugger>() else {
                return Then::Go;
            };
            for id in dbg.dropped.drain(..) {
                ffi::lua_unref(state, id);
            }
            if !dbg.reached.iter().any(|(m, l)| *l == line && m == module) {
                dbg.reached.push((module.to_owned(), line));
            }
            if evaluating {
                return Then::Go;
            }
            let Some(slots) = dbg.modules.get(module) else {
                return Then::Go;
            };
            slots
                .iter()
                .enumerate()
                .filter(|(_, s)| s.asked.enabled && s.state.placed == Some(line))
                .map(|(slot, s)| Ask {
                    slot,
                    condition: s.asked.condition.clone(),
                    action: s.asked.action.clone(),
                })
                .collect()
        };
        let mut stop = false;
        for ask in asks {
            if let Some(condition) = &ask.condition {
                match evaluate(state, lua, module, ask.slot, true, condition) {
                    Ok(holds) if truthy(&holds) => {}
                    Ok(_) => continue,
                    Err(e) => {
                        note_error(lua, module, ask.slot, e);
                        continue;
                    }
                }
            }
            let admitted = {
                let Some(mut dbg) = lua.app_data_mut::<Debugger>() else {
                    return Then::Go;
                };
                let Some(s) = dbg
                    .modules
                    .get_mut(module)
                    .and_then(|m| m.get_mut(ask.slot))
                else {
                    continue;
                };
                s.state.hits += 1;
                let admits = s.asked.hits.admits(s.state.hits);
                dbg.version += 1;
                admits
            };
            if !admitted {
                continue;
            }
            match &ask.action {
                BreakAction::Log(text) => match evaluate(state, lua, module, ask.slot, false, text)
                {
                    Ok(Evaluated::Text(t)) => {
                        if let Some(mut dbg) = lua.app_data_mut::<Debugger>() {
                            if let Some(s) = dbg
                                .modules
                                .get_mut(module)
                                .and_then(|m| m.get_mut(ask.slot))
                            {
                                s.state.last = Some(t.clone());
                                s.state.error = None;
                            }
                            dbg.version += 1;
                            dbg.logged.push(Logged {
                                module: module.to_owned(),
                                line,
                                text: t,
                            });
                        }
                    }
                    Ok(_) => {}
                    Err(e) => note_error(lua, module, ask.slot, e),
                },
                BreakAction::Pause => ask_pause(lua, module, line),
                BreakAction::Stop => stop = true,
            }
        }
        // After the line's logpoints, once however many stops it has; or where a step
        // reaches a breakpoint's line (its first instruction is the breakpoint, which the
        // stepping loop does not step).
        if stop || step_due(state, lua) {
            stop_at(state, lua, module, line)
        } else {
            Then::Go
        }
    }
}

/// A pause after the tick, asked at `module`'s `line` unless one was asked already.
fn ask_pause(lua: &Lua, module: &str, line: u32) {
    if let Some(mut dbg) = lua.app_data_mut::<Debugger>()
        && dbg.pause.is_none()
    {
        dbg.pause = Some((module.to_owned(), line));
    }
}

/// A stop acted on at `module`'s `line`: the host's handler waits, the call's time budget
/// held meanwhile; a pause after the tick where it cannot. What the callback does then.
///
/// # Safety
///
/// As [`debugbreak`]'s.
unsafe fn stop_at(state: *mut ffi::lua_State, lua: &Lua, module: &str, line: u32) -> Then {
    if ABORTING.get() {
        return Then::Abort;
    }
    // A break asked is met by any stop.
    if let Some(mut d) = lua.app_data_mut::<Debugger>() {
        d.break_next = false;
    }
    // SAFETY: as the caller says.
    let resume = wait(lua, || unsafe { describe(state, lua, module, line, 0) });
    // SAFETY: as the caller says.
    unsafe {
        match resume {
            Some(Resume::Abort) => {
                ABORTING.set(true);
                if let Some(mut d) = lua.app_data_mut::<Debugger>() {
                    d.reset_step();
                }
                Then::Abort
            }
            Some(Resume::Continue) => end_steps(lua),
            Some(how) => step_from(state, lua, how),
            None => {
                ask_pause(lua, module, line);
                end_steps(lua)
            }
        }
    }
}

/// The host's handler waits on the stop `stopped` makes (made only when there is a
/// handler), the call's time budget held meanwhile: how the developer goes on; none where
/// the host cannot wait. The handler is out of the state while it runs: the editor's frames
/// meanwhile read and set breakpoints.
fn wait(lua: &Lua, stopped: impl FnOnce() -> Stopped) -> Option<Resume> {
    let handler = lua
        .app_data_mut::<Debugger>()
        .and_then(|mut d| d.handler.take());
    handler.and_then(|mut handler| {
        let stopped = stopped();
        let held = crate::hold_budget();
        let resume = handler(&stopped);
        crate::release_budget(held);
        if let Some(mut d) = lua.app_data_mut::<Debugger>()
            && d.handler.is_none()
        {
            d.handler = Some(handler);
        }
        resume
    })
}

/// The message handler of hooks called while errors the script does not catch stop
/// ([`ErrorStops::uncaught`]): Luau calls it where the error was raised, the frames that
/// raised it still below its own, and it stops there ([`error_stop`]). Its result is the
/// error's value, or the abort's where the developer ended the call there.
///
/// # Safety
///
/// Luau calls it as a protected call's message handler, the error's value its argument.
pub(crate) unsafe extern "C-unwind" fn error_handler(state: *mut ffi::lua_State) -> c_int {
    // A panic never crosses Luau's frames: no stop then.
    // SAFETY: as the caller says: the error's value is at the top of a running thread.
    let aborted = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
        let message = error_message(state, -1);
        error_stop(state, message, false)
    }))
    .unwrap_or(false);
    if aborted {
        // SAFETY: a C function has room for a value more.
        unsafe {
            ffi::lua_pushlstring_(
                state,
                crate::ABORT_MARKER.as_ptr().cast(),
                crate::ABORT_MARKER.len(),
            );
        }
    }
    1
}

/// Luau's protected-error callback: an error a `pcall` in the script caught, its frames
/// still there, stops where errors caught stop ([`ErrorStops::caught`]). Ended there, the
/// call ends at its next interrupt (the callback may not raise).
///
/// # Safety
///
/// Luau calls it with the thread whose protected call failed, the error's value on top.
pub(crate) unsafe extern "C-unwind" fn protected_error(state: *mut ffi::lua_State) {
    if !ERRORS.get().caught || EVALUATING.get() || ABORTING.get() {
        return;
    }
    // SAFETY: as the caller says.
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
        let message = error_message(state, -1);
        error_stop(state, message, true)
    }));
}

/// An error at `state`'s frames as they raised it: a stop at the innermost script line,
/// where errors of its kind (`caught` or not) stop and that line's have not stopped since
/// the states were reset; where the host cannot wait, a pause after the tick. Whether the
/// developer ended the call there.
///
/// # Safety
///
/// `state` is a thread whose frames are as the error left them (inside the message handler
/// or the protected-error callback, or after its `lua_resume` failed), with room for four
/// values; no function is called on it.
pub(crate) unsafe fn error_stop(state: *mut ffi::lua_State, message: String, caught: bool) -> bool {
    if ABORTING.get() || EVALUATING.get() || message.contains(crate::ABORT_MARKER) {
        return false;
    }
    // Luau calls the handler for memory errors too: the stop could not be described.
    if message == "not enough memory" {
        return false;
    }
    // SAFETY: as the caller says; `ar` is filled for each level, its source a C string Luau
    // keeps.
    unsafe {
        let lua = Lua::get_or_init_from_ptr(state);
        let wanted = lua.app_data_ref::<Debugger>().is_some_and(|d| {
            if caught {
                d.errors.caught
            } else {
                d.errors.uncaught
            }
        });
        if !wanted {
            return false;
        }
        // The innermost frame in a script: `error` and the host's functions are C.
        let mut at = None;
        for level in 0..64 {
            let mut ar: ffi::lua_Debug = core::mem::zeroed();
            if ffi::lua_getinfo(state, level, c"sl".as_ptr(), &mut ar) == 0 {
                break;
            }
            let source =
                (!ar.source.is_null()).then(|| CStr::from_ptr(ar.source).to_string_lossy());
            if let (Some(source), Ok(line)) = (source, u32::try_from(ar.currentline))
                && let Some(module) = source.strip_prefix('=')
                && line > 0
            {
                at = Some((level, module.to_owned(), line));
                break;
            }
        }
        let Some((level, module, line)) = at else {
            return false;
        };
        {
            let Some(mut d) = lua.app_data_mut::<Debugger>() else {
                return false;
            };
            if d.errors_seen
                .iter()
                .any(|(m, l)| *l == line && *m == module)
            {
                return false;
            }
            d.errors_seen.push((module.clone(), line));
            d.break_next = false;
        }
        let (_, message) = crate::split_location(&module, &message);
        let message = message.to_owned();
        let resume = wait(lua, || {
            let mut stopped = describe(state, lua, &module, line, level);
            stopped.steppable = false;
            stopped.error = Some(ErrorStop { message, caught });
            stopped
        });
        // No step goes on past an error, nor single-stepping.
        if let Some(mut d) = lua.app_data_mut::<Debugger>() {
            d.reset_step();
        }
        match resume {
            Some(Resume::Abort) => {
                ABORTING.set(true);
                true
            }
            Some(_) => false,
            None => {
                ask_pause(lua, &module, line);
                false
            }
        }
    }
}

/// An error's value at `idx` as its message's first line: a string as it is, the host's
/// error (mlua's, a userdata of type `error`) as it formats itself, anything else by its
/// type.
///
/// # Safety
///
/// `idx` is a valid index of a running thread `state`, with room for two more values.
pub(crate) unsafe fn error_message(state: *mut ffi::lua_State, idx: c_int) -> String {
    // SAFETY: as the caller says; what is pushed is popped after it is read.
    unsafe {
        let idx = ffi::lua_absindex(state, idx);
        let text = match ffi::lua_type(state, idx) {
            ffi::LUA_TSTRING => text_at(state, idx).unwrap_or_default(),
            ffi::LUA_TNUMBER => number_text(ffi::lua_tonumber(state, idx)),
            ffi::LUA_TUSERDATA if value_text(state, idx) == "error" => {
                // Its `__tostring` is mlua's, in Rust.
                let mut len = 0usize;
                let p = ffi::luaL_tolstring(state, idx, &mut len);
                let text = (!p.is_null()).then(|| {
                    String::from_utf8_lossy(core::slice::from_raw_parts(p.cast::<u8>(), len))
                        .into_owned()
                });
                ffi::lua_settop(state, -2);
                text.unwrap_or_else(|| String::from("an error"))
            }
            other => {
                let name = CStr::from_ptr(ffi::lua_typename(state, other)).to_string_lossy();
                format!("(error object is a {name} value)")
            }
        };
        text.lines().next().unwrap_or_default().to_owned()
    }
}

/// Whether the script stopped on `state` can be stepped from there.
///
/// # Safety
///
/// `state` is a running thread.
unsafe fn steppable(state: *mut ffi::lua_State, lua: &Lua) -> bool {
    let ours = lua
        .app_data_ref::<Debugger>()
        .and_then(|d| d.hook_thread)
        .is_some_and(|t| t.as_ptr() == state);
    // SAFETY: as the caller says.
    ours && unsafe { ffi::lua_isyieldable(state) } != 0
}

/// The script goes on with no step: single-stepping off from the next hook on. The
/// stepping loop runs the rest of this one: a break out of it would resume the frame
/// where it is, and a native frame cannot be entered there (see the module's
/// documentation).
///
/// # Safety
///
/// As [`debugbreak`]'s.
unsafe fn end_steps(lua: &Lua) -> Then {
    if let Some(mut d) = lua.app_data_mut::<Debugger>() {
        d.reset_step();
    }
    Then::Go
}

/// A step from the stop on `state`, `how` far: single-stepping on, and into the stepping
/// loop at the next resume. Where it cannot step, the script goes on.
///
/// # Safety
///
/// As [`debugbreak`]'s.
unsafe fn step_from(state: *mut ffi::lua_State, lua: &Lua, how: Resume) -> Then {
    // SAFETY: as the caller says.
    unsafe {
        if !steppable(state, lua) {
            return end_steps(lua);
        }
        let mut ar: ffi::lua_Debug = core::mem::zeroed();
        ffi::lua_getinfo(state, 0, c"sl".as_ptr(), &mut ar);
        let step = Step {
            how,
            depth: ffi::lua_stackdepth(state),
            source: ar.source,
            line: ar.currentline,
            call: HOOK_CALLS.get(),
        };
        let Some(mut d) = lua.app_data_mut::<Debugger>() else {
            return Then::Go;
        };
        d.step = Some(step);
        d.want_thread();
        if d.single {
            // In the stepping loop already: it goes on there.
            return Then::Go;
        }
        d.single = true;
        ffi::lua_singlestep(state, 1);
        Then::Break
    }
}

/// The frame at level `from` of `state`, the stopped one, as text: its locals, its
/// function's upvalues, and the stack from it out to the host's call.
///
/// # Safety
///
/// `state` is a thread inside one of Luau's debug callbacks, or one whose frames an error
/// left (no function is called on it).
unsafe fn describe(
    state: *mut ffi::lua_State,
    lua: &Lua,
    module: &str,
    line: u32,
    from: c_int,
) -> Stopped {
    let mut stopped = Stopped {
        module: module.to_owned(),
        line,
        locals: Vec::new(),
        upvalues: Vec::new(),
        stack: Vec::new(),
        // SAFETY: as the caller says.
        steppable: unsafe { steppable(state, lua) },
        error: None,
    };
    // SAFETY: as the caller says; each value pushed is read, then popped, and the stack is
    // put back as it was.
    unsafe {
        if ffi::lua_checkstack(state, 4) == 0 {
            return stopped;
        }
        for level in from..from + 64 {
            let mut ar: ffi::lua_Debug = core::mem::zeroed();
            if ffi::lua_getinfo(state, level, c"sln".as_ptr(), &mut ar) == 0 {
                break;
            }
            let text = |p: *const core::ffi::c_char| {
                (!p.is_null()).then(|| CStr::from_ptr(p).to_string_lossy().into_owned())
            };
            let source = text(ar.source).unwrap_or_default();
            let (locals, upvalues) = frame_variables(state, level);
            let line = u32::try_from(ar.currentline).ok().filter(|&l| l > 0);
            let mut shape: ffi::lua_Debug = core::mem::zeroed();
            ffi::lua_getinfo(state, level, c"a".as_ptr(), &mut shape);
            let native = line.is_some() && shape.nparams > 0 && locals.is_empty();
            stopped.stack.push(Frame {
                source: source.strip_prefix('=').unwrap_or(&source).to_owned(),
                line,
                function: text(ar.name),
                locals,
                upvalues,
                native,
            });
        }
        if let Some(frame) = stopped.stack.first() {
            stopped.locals.clone_from(&frame.locals);
            stopped.upvalues.clone_from(&frame.upvalues);
        }
    }
    stopped
}

/// The locals and the function's upvalues of the frame at `level` of `state`, as text.
///
/// # Safety
///
/// `state` is a running thread inside one of Luau's debug callbacks, with room for four
/// more values; the stack is put back as it was.
unsafe fn frame_variables(
    state: *mut ffi::lua_State,
    level: c_int,
) -> (Vec<Variable>, Vec<Variable>) {
    let (mut locals, mut upvalues) = (Vec::new(), Vec::new());
    // SAFETY: as the caller says; each value pushed is read, then popped.
    unsafe {
        let top = ffi::lua_gettop(state);
        for n in 1..=200 {
            let name = ffi::lua_getlocal(state, level, n);
            if name.is_null() {
                break;
            }
            let name = CStr::from_ptr(name).to_string_lossy().into_owned();
            if is_name(&name) {
                let value = value_text(state, -1);
                locals.push(Variable { name, value });
            }
            ffi::lua_settop(state, -2);
        }
        let mut ar: ffi::lua_Debug = core::mem::zeroed();
        if ffi::lua_getinfo(state, level, c"f".as_ptr(), &mut ar) != 0 {
            let function = ffi::lua_gettop(state);
            for n in 1..=255 {
                let name = ffi::lua_getupvalue(state, function, n);
                if name.is_null() {
                    break;
                }
                let name = CStr::from_ptr(name).to_string_lossy().into_owned();
                if is_name(&name) {
                    let value = value_text(state, -1);
                    upvalues.push(Variable { name, value });
                }
                ffi::lua_settop(state, -2);
            }
        }
        ffi::lua_settop(state, top);
    }
    (locals, upvalues)
}

/// The value at `idx` as text, never through its metamethods: a string quoted and cut, a
/// table with its length, a userdata by its metatable's `__type`.
///
/// # Safety
///
/// `idx` is a valid index of `state`, with room for two more values.
unsafe fn value_text(state: *mut ffi::lua_State, idx: c_int) -> String {
    // SAFETY: as the caller says; a metatable and its field are popped after they are read.
    unsafe {
        let idx = ffi::lua_absindex(state, idx);
        match ffi::lua_type(state, idx) {
            ffi::LUA_TNIL => String::from("nil"),
            ffi::LUA_TBOOLEAN => String::from(if ffi::lua_toboolean(state, idx) != 0 {
                "true"
            } else {
                "false"
            }),
            ffi::LUA_TNUMBER => number_text(ffi::lua_tonumber(state, idx)),
            ffi::LUA_TSTRING => {
                let text = text_at(state, idx).unwrap_or_default();
                let mut cut: String = text.chars().take(VALUE_CHARS).collect();
                if cut.len() < text.len() {
                    cut.push('\u{2026}');
                }
                format!("{cut:?}")
            }
            ffi::LUA_TVECTOR => {
                let v = ffi::lua_tovector(state, idx);
                if v.is_null() {
                    String::from("vector")
                } else {
                    let v = core::slice::from_raw_parts(v, 3);
                    let [x, y, z] = [v[0], v[1], v[2]].map(|c| number_text(f64::from(c)));
                    format!("({x}, {y}, {z})")
                }
            }
            ffi::LUA_TTABLE => match ffi::lua_objlen(state, idx) {
                0 => String::from("table"),
                n => format!("table #{n}"),
            },
            ffi::LUA_TFUNCTION => String::from("function"),
            ffi::LUA_TUSERDATA => {
                let mut name = None;
                if ffi::lua_getmetatable(state, idx) != 0 {
                    if ffi::lua_rawgetfield(state, -1, c"__type".as_ptr()) == ffi::LUA_TSTRING {
                        name = text_at(state, -1);
                    }
                    ffi::lua_settop(state, -3);
                }
                name.unwrap_or_else(|| String::from("userdata"))
            }
            ffi::LUA_TLIGHTUSERDATA => String::from("userdata"),
            ffi::LUA_TTHREAD => String::from("thread"),
            ffi::LUA_TBUFFER => format!("buffer #{}", ffi::lua_objlen(state, idx)),
            _ => String::from("?"),
        }
    }
}

/// The characters of a string a value's text shows.
const VALUE_CHARS: usize = 60;

/// A number as Luau prints it: whole ones without a fraction, the rest the shortest that
/// reads back.
fn number_text(n: f64) -> String {
    if n.is_nan() {
        String::from("nan")
    } else if n.is_infinite() {
        String::from(if n > 0.0 { "inf" } else { "-inf" })
    } else if n == n.trunc() && n.abs() < 1e15 {
        format!("{n:.0}")
    } else if n == n.trunc() {
        format!("{n:e}")
    } else {
        format!("{n}")
    }
}

/// What an evaluation gave.
enum Evaluated {
    Truth(bool),
    Text(String),
}

fn truthy(e: &Evaluated) -> bool {
    matches!(e, Evaluated::Truth(true))
}

fn note_error(lua: &Lua, module: &str, slot: usize, error: String) {
    if let Some(mut dbg) = lua.app_data_mut::<Debugger>() {
        if let Some(s) = dbg.modules.get_mut(module).and_then(|m| m.get_mut(slot)) {
            s.state.error = Some(error);
        }
        dbg.version += 1;
    }
}

/// Whether `name` can be a parameter (Luau's internal locals are named in parentheses).
fn is_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Runs breakpoint `slot`'s condition (`condition`) or text (else) in the frame at level 0
/// of `state`: the frame's locals, then its function's upvalues, by name (a later local
/// shadows an earlier one and an upvalue), the module's globals.
///
/// # Safety
///
/// `state` is a running thread inside Luau's breakpoint callback.
unsafe fn evaluate(
    state: *mut ffi::lua_State,
    lua: &Lua,
    module: &str,
    slot: usize,
    condition: bool,
    code: &str,
) -> Result<Evaluated, String> {
    // SAFETY: as the caller says; every value is pushed onto `state`'s stack and the stack
    // is put back as it was before returning.
    unsafe {
        let top = ffi::lua_gettop(state);
        if ffi::lua_checkstack(state, 300) == 0 {
            return Err(String::from("no room on the script's stack"));
        }
        // The frame's locals and its function's upvalues: names and where their values lie.
        let mut names: Vec<(String, c_int)> = Vec::new();
        for n in 1..=200 {
            let name = ffi::lua_getlocal(state, 0, n);
            if name.is_null() {
                break;
            }
            let name = CStr::from_ptr(name).to_string_lossy().into_owned();
            let at = ffi::lua_gettop(state);
            if is_name(&name) {
                names.retain(|(n, _)| *n != name);
                names.push((name, at));
            }
        }
        let mut ar: ffi::lua_Debug = core::mem::zeroed();
        ffi::lua_getinfo(state, 0, c"f".as_ptr(), &mut ar);
        let function = ffi::lua_gettop(state);
        for n in 1..=255 {
            let name = ffi::lua_getupvalue(state, function, n);
            if name.is_null() {
                break;
            }
            let name = CStr::from_ptr(name).to_string_lossy().into_owned();
            let at = ffi::lua_gettop(state);
            if is_name(&name) && !names.iter().any(|(n, _)| *n == name) {
                names.push((name, at));
            }
        }
        let key = CodeKey {
            module: module.to_owned(),
            slot,
            condition,
            names: names
                .iter()
                .map(|(n, _)| n.as_str())
                .collect::<Vec<_>>()
                .join(","),
        };
        let compiled = lua
            .app_data_ref::<Debugger>()
            .and_then(|d| d.code.get(&key).copied());
        let id = match compiled {
            Some(id) => id,
            None => {
                // Compiled, loaded and run on this thread: the outer function makes the one
                // kept.
                let parameters = key.names.as_str();
                let body = if condition {
                    format!("return ({code})")
                } else {
                    format!("return `{}`", code.replace('`', "\\`"))
                };
                let source = format!("return function({parameters}) {body} end");
                let bytecode = mlua::chunk::Compiler::new().compile(&source);
                let bytecode = match bytecode {
                    Ok(b) => b,
                    Err(e) => {
                        ffi::lua_settop(state, top);
                        return Err(e.to_string());
                    }
                };
                let loaded = ffi::luau_load(
                    state,
                    c"=debugger".as_ptr(),
                    bytecode.as_ptr().cast(),
                    bytecode.len(),
                    0,
                );
                if loaded != 0 || ffi::lua_pcall(state, 0, 1, 0) != 0 {
                    let why =
                        text_at(state, -1).unwrap_or_else(|| String::from("does not compile"));
                    ffi::lua_settop(state, top);
                    return Err(why);
                }
                let id = ffi::lua_ref(state, -1);
                ffi::lua_settop(state, ffi::lua_gettop(state) - 1);
                if let Some(mut dbg) = lua.app_data_mut::<Debugger>() {
                    dbg.code.insert(key, id);
                }
                id
            }
        };
        // The compiled function, with the frame's environment, called with the values.
        ffi::lua_rawgeti_(state, ffi::LUA_REGISTRYINDEX, id);
        let call = ffi::lua_gettop(state);
        ffi::lua_getfenv(state, function);
        ffi::lua_setfenv(state, call);
        for &(_, at) in &names {
            ffi::lua_pushvalue(state, at);
        }
        EVALUATING.set(true);
        let status = ffi::lua_pcall(state, names.len() as c_int, 1, 0);
        EVALUATING.set(false);
        let result = if status != 0 {
            Err(text_at(state, -1).unwrap_or_else(|| String::from("an error")))
        } else if condition {
            Ok(Evaluated::Truth(ffi::lua_toboolean(state, -1) != 0))
        } else {
            Ok(Evaluated::Text(text_at(state, -1).unwrap_or_default()))
        };
        ffi::lua_settop(state, top);
        result
    }
}

/// The string (or number) at `idx`, as text.
///
/// # Safety
///
/// `idx` is a valid index of `state`.
unsafe fn text_at(state: *mut ffi::lua_State, idx: c_int) -> Option<String> {
    // SAFETY: as the caller says; Luau keeps the bytes while the value is on the stack.
    unsafe {
        let mut len = 0usize;
        let p = ffi::lua_tolstring(state, idx, &mut len);
        (!p.is_null()).then(|| {
            String::from_utf8_lossy(core::slice::from_raw_parts(p.cast::<u8>(), len)).into_owned()
        })
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        BreakAction, Breakpoint, ErrorStop, ErrorStops, Frame, Hits, Logged, Resume, ScriptError,
        ScriptSettings, ScriptVm, Stopped, Variable,
    };
    use core::ffi::c_void;
    use engine_platform::fiber::{self, Fiber, Ran};
    use std::cell::{Cell, RefCell};
    use std::collections::VecDeque;
    use std::time::Duration;

    const DOOR: &str = "
local BASE = 10
local M = {}
function M.tick(speed)
    -- doubled
    local doubled = speed * 2
    return doubled + BASE
end
return M
";

    fn log(line: u32, text: &str) -> Breakpoint {
        Breakpoint {
            line,
            action: BreakAction::Log(text.to_owned()),
            condition: None,
            hits: Hits::Every,
            enabled: true,
        }
    }

    fn texts(vm: &ScriptVm) -> Vec<String> {
        vm.take_logged().into_iter().map(|l| l.text).collect()
    }

    fn stop(line: u32) -> Breakpoint {
        Breakpoint {
            line,
            action: BreakAction::Stop,
            condition: None,
            hits: Hits::Every,
            enabled: true,
        }
    }

    thread_local! {
        static AT: RefCell<Option<Stopped>> = const { RefCell::new(None) };
        static ANSWER: Cell<Resume> = const { Cell::new(Resume::Continue) };
    }

    /// The tests' host: keeps the stop and gives the thread back, where a fiber runs the
    /// call; it cannot wait elsewhere.
    fn wait_on_fiber(stopped: &Stopped) -> Option<Resume> {
        AT.set(Some(stopped.clone()));
        fiber::suspend().then(|| ANSWER.get())
    }

    /// A call into module `module`'s function on a fiber.
    struct Call {
        vm: *const ScriptVm,
        module: &'static str,
        f: mlua::Function,
        arg: f64,
        result: Option<Result<f64, ScriptError>>,
    }

    unsafe fn call(context: *mut c_void) {
        // SAFETY: the test's `Call` and its VM, alive and only read until the job is done.
        let call = unsafe { &mut *context.cast::<Call>() };
        // SAFETY: as above.
        let vm = unsafe { &*call.vm };
        call.result = Some(vm.call_function(call.module, &call.f, call.arg));
    }

    thread_local! {
        /// Each stop's line and whether it could be stepped from, and the answers given in
        /// turn (then Continue).
        static SEEN: RefCell<Vec<(u32, bool)>> = const { RefCell::new(Vec::new()) };
        static STACKS: RefCell<Vec<Vec<Frame>>> = const { RefCell::new(Vec::new()) };
        static STOPS: RefCell<Vec<Stopped>> = const { RefCell::new(Vec::new()) };
        static ANSWERS: RefCell<VecDeque<Resume>> = const { RefCell::new(VecDeque::new()) };
    }

    /// The stepping tests' host: notes each stop and answers the next answer.
    fn answer_in_turn(stopped: &Stopped) -> Option<Resume> {
        SEEN.with_borrow_mut(|s| s.push((stopped.line, stopped.steppable)));
        STACKS.with_borrow_mut(|s| s.push(stopped.stack.clone()));
        STOPS.with_borrow_mut(|s| s.push(stopped.clone()));
        if !fiber::suspend() {
            return None;
        }
        Some(
            ANSWERS
                .with_borrow_mut(VecDeque::pop_front)
                .unwrap_or_default(),
        )
    }

    /// `call` run on `fiber` through every stop, answered in turn: its result.
    fn run_through(fiber: &Fiber, c: &mut Call, answers: &[Resume]) -> Result<f64, ScriptError> {
        ANSWERS.set(answers.iter().copied().collect());
        // SAFETY: `c` and its VM outlive the job, only read meanwhile.
        let mut ran = unsafe { fiber.run(call, core::ptr::from_mut(c).cast()) };
        while ran == Ran::Suspended {
            ran = fiber.resume();
        }
        c.result.take().unwrap()
    }

    fn named(variables: &[Variable]) -> Vec<(&str, &str)> {
        variables
            .iter()
            .map(|v| (v.name.as_str(), v.value.as_str()))
            .collect()
    }

    #[test]
    fn a_logpoint_reads_the_frames_names_and_moves_to_a_line_with_code() {
        let mut vm = ScriptVm::new(ScriptSettings::default()).unwrap();
        vm.load_module("door", DOOR).unwrap();
        // Set after the module ran: its function has it all the same.
        vm.set_breakpoints(
            "door",
            vec![log(
                5,
                "speed {speed} doubled {doubled} base {BASE} pi {math.floor(math.pi)}",
            )],
        );
        assert_eq!(
            vm.breakpoint_states("door")[0].placed,
            Some(6),
            "a comment's line moves"
        );
        assert_eq!(vm.call::<_, f64>("door", "tick", 3.0).unwrap(), 16.0);
        // On line 6 the local is not assigned yet.
        assert_eq!(texts(&vm), ["speed 3 doubled nil base 10 pi 3"]);
        vm.set_breakpoints("door", vec![log(7, "doubled {doubled}")]);
        vm.call::<_, f64>("door", "tick", 4.0).unwrap();
        let logged = vm.take_logged();
        assert_eq!(
            logged,
            [Logged {
                module: String::from("door"),
                line: 7,
                text: String::from("doubled 8"),
            }]
        );
        let state = &vm.breakpoint_states("door")[0];
        assert_eq!((state.hits, state.last.as_deref()), (1, Some("doubled 8")));
        // Taken away: nothing more.
        vm.set_breakpoints("door", Vec::new());
        vm.call::<_, f64>("door", "tick", 5.0).unwrap();
        assert!(vm.take_logged().is_empty());
    }

    #[test]
    fn conditions_hits_pauses_and_errors() {
        let mut vm = ScriptVm::new(ScriptSettings::default()).unwrap();
        vm.load_module("door", DOOR).unwrap();
        let mut every_second = log(7, "speed {speed}");
        every_second.condition = Some(String::from("speed > 2"));
        every_second.hits = Hits::EveryNth(2);
        let pause = Breakpoint {
            line: 7,
            action: BreakAction::Pause,
            condition: Some(String::from("doubled >= 10")),
            hits: Hits::Every,
            enabled: true,
        };
        vm.set_breakpoints("door", vec![every_second, pause]);
        for speed in [1.0, 3.0, 4.0] {
            vm.call::<_, f64>("door", "tick", speed).unwrap();
        }
        // Held for 3 and 4 (hits 1 and 2): the second logs; no pause yet.
        assert_eq!(texts(&vm), ["speed 4"]);
        assert!(vm.take_pause().is_none());
        vm.call::<_, f64>("door", "tick", 5.0).unwrap();
        assert!(vm.pause_asked());
        assert_eq!(
            vm.take_pause(),
            Some((String::from("door"), 7)),
            "doubled 10"
        );
        assert!(vm.take_pause().is_none(), "taken");
        assert!(!vm.pause_asked());
        // A condition that does not compile, a text that fails: the error is kept, the
        // script goes on.
        let mut broken = log(7, "{speed + nil}");
        broken.condition = Some(String::from("speed >"));
        vm.set_breakpoints("door", vec![broken, log(7, "{speed + nil}")]);
        assert_eq!(vm.call::<_, f64>("door", "tick", 1.0).unwrap(), 12.0);
        let states = vm.breakpoint_states("door");
        assert!(
            states[0].error.is_some() && states[1].error.is_some(),
            "{states:?}"
        );
        assert!(vm.take_logged().is_empty());
    }

    #[test]
    fn breakpoints_follow_their_module_through_reloads_and_offs() {
        let mut vm = ScriptVm::new(ScriptSettings::default()).unwrap();
        // Set before the module loads: placed when it does.
        let mut off = log(7, "off");
        off.enabled = false;
        vm.set_breakpoints("door", vec![log(7, "first {speed}"), off]);
        vm.load_module("door", DOOR).unwrap();
        vm.call::<_, f64>("door", "tick", 1.0).unwrap();
        assert_eq!(texts(&vm), ["first 1"]);
        // Reloaded with a change: the breakpoint is in the new code.
        vm.load_module("door", &DOOR.replace("speed * 2", "speed * 3"))
            .unwrap();
        assert_eq!(vm.call::<_, f64>("door", "tick", 2.0).unwrap(), 16.0);
        assert_eq!(texts(&vm), ["first 2"]);
        assert_eq!(
            vm.breakpoint_states("door")[1].placed,
            None,
            "off: not placed"
        );
    }

    #[test]
    fn a_stop_waits_with_its_frame_then_goes_on_or_ends_the_call() {
        let settings = ScriptSettings {
            call_budget: Duration::from_millis(20),
            ..ScriptSettings::default()
        };
        let mut vm = ScriptVm::new(settings).unwrap();
        vm.load_module("door", DOOR).unwrap();
        vm.set_breakpoints("door", vec![stop(7)]);
        // No handler: a pause point.
        assert_eq!(vm.call::<_, f64>("door", "tick", 3.0).unwrap(), 16.0);
        assert_eq!(vm.take_pause(), Some((String::from("door"), 7)));
        // A handler, but no fiber to wait on: a pause point again.
        vm.set_stop_handler(Some(Box::new(wait_on_fiber)));
        assert_eq!(vm.call::<_, f64>("door", "tick", 3.0).unwrap(), 16.0);
        assert_eq!(vm.take_pause(), Some((String::from("door"), 7)));
        // On a fiber: the call waits at line 7, with its frame.
        let fiber = Fiber::new(1 << 20).unwrap();
        let tick: mlua::Function = vm.exports("door").unwrap().get("tick").unwrap();
        let mut c = Call {
            vm: &raw const vm,
            module: "door",
            f: tick,
            arg: 3.0,
            result: None,
        };
        // SAFETY: `c` and the VM outlive the job, only read meanwhile.
        let ran = unsafe { fiber.run(call, (&raw mut c).cast()) };
        assert_eq!(ran, Ran::Suspended);
        assert!(c.result.is_none(), "it waits");
        let at = AT.take().unwrap();
        assert_eq!((at.module.as_str(), at.line), ("door", 7));
        assert_eq!(named(&at.locals), [("speed", "3"), ("doubled", "6")]);
        assert_eq!(named(&at.upvalues), [("BASE", "10")]);
        assert_eq!(
            (
                at.stack[0].source.as_str(),
                at.stack[0].line,
                at.stack[0].function.as_deref()
            ),
            ("door", Some(7), Some("tick"))
        );
        assert_eq!(at.stack[0].locals, at.locals, "the stopped frame's");
        // The editor's frames meanwhile: they read the breakpoints and change them, and
        // the time past the call's budget does not count.
        let debugger = vm.debugger();
        assert_eq!(debugger.breakpoint_states("door")[0].hits, 3);
        debugger.set_breakpoints("door", vec![log(7, "doubled {doubled}"), stop(7)]);
        #[allow(clippy::disallowed_methods)] // a test's wait: no precision needed
        std::thread::sleep(Duration::from_millis(40));
        ANSWER.set(Resume::Continue);
        assert_eq!(fiber.resume(), Ran::Done);
        assert_eq!(c.result.take(), Some(Ok(16.0)));
        assert!(
            vm.take_logged().is_empty(),
            "set after its line was reached"
        );
        // The next call logs, then stops: ended there, the call fails as aborted.
        c.arg = 4.0;
        // SAFETY: as above.
        let ran = unsafe { fiber.run(call, (&raw mut c).cast()) };
        assert_eq!(ran, Ran::Suspended);
        assert_eq!(texts(&vm), ["doubled 8"]);
        ANSWER.set(Resume::Abort);
        assert_eq!(fiber.resume(), Ran::Done);
        assert_eq!(
            c.result.take(),
            Some(Err(ScriptError::Aborted {
                module: String::from("door")
            }))
        );
        // Calls fail at once until the host ends the abort; then the VM goes on.
        vm.set_breakpoints("door", Vec::new());
        assert!(matches!(
            vm.call::<_, f64>("door", "tick", 5.0),
            Err(ScriptError::Aborted { .. })
        ));
        vm.debugger().end_abort();
        assert_eq!(vm.call::<_, f64>("door", "tick", 5.0).unwrap(), 20.0);
    }

    const STEPS: &str = "
local M = {}
local function double(x)
    local y = x * 2
    return y
end
function M.tick(speed)
    local a = speed + 1
    local b = double(a)
    local c = b + 1
    return c
end
function M.sorted(n)
    local t = {3, 1, 2}
    table.sort(t, function(p, q)
        return p < q
    end)
    return t[1] + n
end
local function mix(x)
    local a = x * 2
    local b = a + x
    local c = b * a
    return c + a
end
function M.mixed(n)
    local r = mix(n)
    return r
end
return M
";

    #[test]
    fn steps_go_over_into_and_out_and_past_the_hooks_end() {
        // Level 1: `double` is called, not inlined into `tick`.
        let settings = ScriptSettings {
            optimization_level: 1,
            ..ScriptSettings::default()
        };
        let mut vm = ScriptVm::new(settings).unwrap();
        vm.load_module("steps", STEPS).unwrap();
        vm.set_debugging(true);
        vm.set_stop_handler(Some(Box::new(answer_in_turn)));
        vm.set_breakpoints("steps", vec![stop(8)]);
        let fiber = Fiber::new(1 << 20).unwrap();
        let tick: mlua::Function = vm.exports("steps").unwrap().get("tick").unwrap();
        let mut c = Call {
            vm: &raw const vm,
            module: "steps",
            f: tick,
            arg: 1.0,
            result: None,
        };
        // Over to the next line, into `double`, over its line, out to the line after the
        // call, over again; then on.
        let answers = [
            Resume::StepOver,
            Resume::StepInto,
            Resume::StepOver,
            Resume::StepOut,
            Resume::StepOver,
        ];
        SEEN.take();
        STACKS.take();
        assert_eq!(run_through(&fiber, &mut c, &answers), Ok(5.0));
        let lines: Vec<u32> = SEEN.take().into_iter().map(|(l, _)| l).collect();
        assert_eq!(lines, [8, 9, 4, 5, 10, 11]);
        // In `double`: its frame, then its caller's, each with its own locals.
        let stack = &STACKS.take()[2];
        let frame = |k: usize| {
            let f: &Frame = &stack[k];
            (f.function.as_deref(), f.line, named(&f.locals))
        };
        assert_eq!(frame(0), (Some("double"), Some(4), vec![("x", "2")]));
        assert_eq!(
            frame(1),
            (Some("tick"), Some(9), vec![("speed", "1"), ("a", "2")])
        );
        assert_eq!(named(&stack[1].upvalues), [("double", "function")]);
        // A step over the hook's last line: the next hook call stops at its first line.
        vm.set_breakpoints("steps", vec![stop(11)]);
        assert_eq!(run_through(&fiber, &mut c, &[Resume::StepOver]), Ok(5.0));
        vm.set_breakpoints("steps", Vec::new());
        assert_eq!(run_through(&fiber, &mut c, &[]), Ok(5.0));
        let seen = SEEN.take();
        assert_eq!(seen, [(11, true), (8, true)], "stopped in the next call");
        // Nothing under way: the hooks run straight.
        assert_eq!(run_through(&fiber, &mut c, &[]), Ok(5.0));
        assert!(SEEN.take().is_empty());
        // Debugging off: called the plain way, a stop there cannot be stepped from.
        vm.set_debugging(false);
        vm.set_breakpoints("steps", vec![stop(8)]);
        assert_eq!(run_through(&fiber, &mut c, &[Resume::StepOver]), Ok(5.0));
        assert_eq!(SEEN.take(), [(8, false)]);
    }

    #[test]
    fn a_stop_under_a_c_function_cannot_be_stepped_from() {
        let mut vm = ScriptVm::new(ScriptSettings::default()).unwrap();
        vm.load_module("steps", STEPS).unwrap();
        vm.set_debugging(true);
        vm.set_stop_handler(Some(Box::new(answer_in_turn)));
        // In `table.sort`'s comparison (its first): a step there goes on instead.
        let mut first = stop(16);
        first.hits = Hits::Nth(1);
        vm.set_breakpoints("steps", vec![first, stop(18)]);
        let fiber = Fiber::new(1 << 20).unwrap();
        let sorted: mlua::Function = vm.exports("steps").unwrap().get("sorted").unwrap();
        let mut c = Call {
            vm: &raw const vm,
            module: "steps",
            f: sorted,
            arg: 10.0,
            result: None,
        };
        SEEN.take();
        let answers = [Resume::StepOver, Resume::StepOver];
        assert_eq!(run_through(&fiber, &mut c, &answers), Ok(11.0));
        // Line 18 returns: the step goes on into the next call, whose first line stops.
        assert_eq!(SEEN.take(), [(16, false), (18, true)]);
        vm.set_breakpoints("steps", Vec::new());
        assert_eq!(run_through(&fiber, &mut c, &[]), Ok(11.0));
        assert_eq!(SEEN.take(), [(14, true)]);
    }

    #[test]
    fn continuing_from_a_step_inside_native_code_finishes_the_hook() {
        // `mix` has no breakpoint: it runs as native code, which a resume halfway through
        // it would enter mid-block. Continue finishes the hook in the stepping loop instead.
        let settings = ScriptSettings {
            optimization_level: 1,
            ..ScriptSettings::default()
        };
        let mut vm = ScriptVm::new(settings).unwrap();
        vm.load_module("steps", STEPS).unwrap();
        vm.set_debugging(true);
        vm.set_stop_handler(Some(Box::new(answer_in_turn)));
        vm.set_breakpoints("steps", vec![stop(27)]);
        let fiber = Fiber::new(1 << 20).unwrap();
        let mixed: mlua::Function = vm.exports("steps").unwrap().get("mixed").unwrap();
        let mut c = Call {
            vm: &raw const vm,
            module: "steps",
            f: mixed,
            arg: 3.0,
            result: None,
        };
        SEEN.take();
        let answers = [Resume::StepInto, Resume::StepOver, Resume::Continue];
        assert_eq!(run_through(&fiber, &mut c, &answers), Ok(60.0));
        assert_eq!(SEEN.take(), [(27, true), (21, true), (22, true)]);
        // The next call runs as before, stopping at its breakpoint only.
        assert_eq!(run_through(&fiber, &mut c, &[]), Ok(60.0));
        assert_eq!(SEEN.take(), [(27, true)]);
    }

    #[test]
    fn a_stops_values_are_read_as_they_are() {
        let mut vm = ScriptVm::new(ScriptSettings::default()).unwrap();
        let source = "
local M = {}
function M.look(n)
    local text = string.rep('ab', 40)
    local v = vector.create(1, 2.5, -3)
    local list = {1, 2, 3}
    local yes, none = true, nil
    local big, part = 2^60, 0.1 + 0.2
    return n + #list
end
return M
";
        vm.load_module("look", source).unwrap();
        vm.set_breakpoints("look", vec![stop(9)]);
        vm.set_stop_handler(Some(Box::new(wait_on_fiber)));
        let fiber = Fiber::new(1 << 20).unwrap();
        let look: mlua::Function = vm.exports("look").unwrap().get("look").unwrap();
        let mut c = Call {
            vm: &raw const vm,
            module: "look",
            f: look,
            arg: 1.0,
            result: None,
        };
        // SAFETY: `c` and the VM outlive the job, only read meanwhile.
        let ran = unsafe { fiber.run(call, (&raw mut c).cast()) };
        assert_eq!(ran, Ran::Suspended);
        let at = AT.take().unwrap();
        let cut = format!("\"{}\u{2026}\"", "ab".repeat(30));
        assert_eq!(
            named(&at.locals),
            [
                ("n", "1"),
                ("text", cut.as_str()),
                ("v", "(1, 2.5, -3)"),
                ("list", "table #3"),
                ("yes", "true"),
                ("none", "nil"),
                ("big", "1.152921504606847e18"),
                ("part", "0.30000000000000004"),
            ]
        );
        assert_eq!(fiber.resume(), Ran::Done);
        assert_eq!(c.result.take(), Some(Ok(4.0)));
    }

    #[test]
    fn a_list_given_again_keeps_the_states_of_what_it_still_asks_for() {
        let mut vm = ScriptVm::new(ScriptSettings::default()).unwrap();
        vm.load_module("door", DOOR).unwrap();
        let kept = log(7, "kept {speed}");
        vm.set_breakpoints("door", vec![kept.clone(), log(6, "dropped")]);
        for speed in [1.0, 2.0] {
            vm.call::<_, f64>("door", "tick", speed).unwrap();
        }
        let before = vm.breakpoints_version();
        // The editor gives the module's whole list when one of them changes.
        vm.set_breakpoints("door", vec![log(6, "new"), kept]);
        assert!(vm.breakpoints_version() > before);
        let states = vm.breakpoint_states("door");
        assert_eq!((states[0].hits, states[0].placed), (0, Some(6)), "new");
        assert_eq!(states[1].hits, 2, "kept: its hits");
        assert_eq!(states[1].last.as_deref(), Some("kept 2"));
        assert_eq!(states[1].placed, Some(7), "and placed again");
        // A new Play: hits and messages back to none, the places kept.
        vm.reset_breakpoint_states();
        let states = vm.breakpoint_states("door");
        assert_eq!(
            (states[1].hits, states[1].last.as_deref(), states[1].placed),
            (0, None, Some(7))
        );
        // Nothing reached: the version stays.
        let still = vm.breakpoints_version();
        assert_eq!(vm.breakpoints_version(), still);
        vm.call::<_, f64>("door", "tick", 3.0).unwrap();
        assert!(vm.breakpoints_version() > still, "hits raise it");
    }

    const FAULTS: &str = "
local M = {}
local function inner(t)
    local y = t.x + 1
    return y
end
function M.tick(n)
    local t = {}
    return inner(t) + n
end
function M.raise(n)
    error(`boom {n}`)
end
function M.guarded(n)
    local ok = pcall(M.raise, n)
    return if ok then 1 else 2
end
function M.spin(n)
    local i = 0
    while true do
        i += n
    end
end
return M
";

    /// A VM with [`FAULTS`] at level 1 (no inlining), a short budget, native code or not,
    /// and the stepping tests' host; a call of its `function` with 1.
    fn faults(function: &str, native: bool) -> (ScriptVm, Call) {
        let settings = ScriptSettings {
            optimization_level: 1,
            call_budget: Duration::from_millis(20),
            native,
            ..ScriptSettings::default()
        };
        let mut vm = ScriptVm::new(settings).unwrap();
        vm.load_module("faults", FAULTS).unwrap();
        vm.set_stop_handler(Some(Box::new(answer_in_turn)));
        let f: mlua::Function = vm.exports("faults").unwrap().get(function).unwrap();
        let c = Call {
            vm: core::ptr::null(),
            module: "faults",
            f,
            arg: 1.0,
            result: None,
        };
        (vm, c)
    }

    /// Each frame's function and line.
    fn frames(stack: &[Frame]) -> Vec<(Option<&str>, Option<u32>)> {
        stack
            .iter()
            .map(|f| (f.function.as_deref(), f.line))
            .collect()
    }

    const UNCAUGHT: ErrorStops = ErrorStops {
        uncaught: true,
        caught: false,
    };

    #[test]
    fn an_error_not_caught_stops_at_its_line_once_a_play_however_hooks_are_called() {
        // Interpreted: every frame's locals can be read.
        let (vm, mut c) = faults("tick", false);
        c.vm = &raw const vm;
        let fiber = Fiber::new(1 << 20).unwrap();
        let debugger = vm.debugger();
        // Off: the error as it was.
        STOPS.take();
        let r = run_through(&fiber, &mut c, &[]);
        assert!(
            matches!(r, Err(ScriptError::Runtime { line: Some(4), .. })),
            "{r:?}"
        );
        assert!(STOPS.take().is_empty());
        // On: it stops at the line before its frames unwind; Continue lets it take its
        // course.
        debugger.set_error_stops(UNCAUGHT);
        assert_eq!(debugger.error_stops(), UNCAUGHT);
        let r = run_through(&fiber, &mut c, &[Resume::Continue]);
        assert!(
            matches!(r, Err(ScriptError::Runtime { line: Some(4), .. })),
            "{r:?}"
        );
        let stops = STOPS.take();
        assert_eq!(stops.len(), 1);
        let at = &stops[0];
        assert_eq!(
            (at.module.as_str(), at.line, at.steppable),
            ("faults", 4, false)
        );
        let e = at.error.as_ref().unwrap();
        assert!(!e.caught && e.message.contains("arithmetic"), "{e:?}");
        assert!(
            !e.message.starts_with("faults:"),
            "without its place: {e:?}"
        );
        assert_eq!(named(&at.locals), [("t", "table")]);
        assert_eq!(
            frames(&at.stack),
            [(Some("inner"), Some(4)), (Some("tick"), Some(9))]
        );
        assert_eq!(named(&at.stack[1].locals), [("n", "1"), ("t", "table")]);
        assert!(at.stack.iter().all(|f| !f.native));
        // Once a line: the next error there takes its course without a stop.
        assert!(run_through(&fiber, &mut c, &[]).is_err());
        assert!(STOPS.take().is_empty());
        // A new Play: it stops again; ended there, the call fails as aborted, and calls
        // fail at once until the host ends the abort.
        debugger.reset_breakpoint_states();
        let r = run_through(&fiber, &mut c, &[Resume::Abort]);
        assert_eq!(
            r,
            Err(ScriptError::Aborted {
                module: String::from("faults")
            })
        );
        assert_eq!(STOPS.take().len(), 1);
        assert!(matches!(
            run_through(&fiber, &mut c, &[]),
            Err(ScriptError::Aborted { .. })
        ));
        debugger.end_abort();
        // On the hook thread (a stop is set elsewhere): the same stop, read from the frames
        // the failed resume left.
        vm.set_debugging(true);
        vm.set_breakpoints("faults", vec![stop(16)]);
        debugger.reset_breakpoint_states();
        let r = run_through(&fiber, &mut c, &[Resume::Continue]);
        assert!(
            matches!(r, Err(ScriptError::Runtime { line: Some(4), .. })),
            "{r:?}"
        );
        let stops = STOPS.take();
        assert_eq!(stops.len(), 1);
        assert_eq!((stops[0].line, stops[0].error.as_ref()), (4, Some(e)));
        assert_eq!(stops[0].stack, at.stack);
        // There too, ended at the error.
        debugger.reset_breakpoint_states();
        let r = run_through(&fiber, &mut c, &[Resume::Abort]);
        assert!(matches!(r, Err(ScriptError::Aborted { .. })), "{r:?}");
        debugger.end_abort();
        // Without a host that waits: a pause after the tick, once.
        vm.set_stop_handler(None);
        debugger.reset_breakpoint_states();
        assert!(vm.call_function::<_, f64>("faults", &c.f, 1.0).is_err());
        assert_eq!(vm.take_pause(), Some((String::from("faults"), 4)));
    }

    #[test]
    fn a_native_frames_locals_are_not_read_and_it_says_so() {
        let (vm, mut c) = faults("tick", true);
        c.vm = &raw const vm;
        let fiber = Fiber::new(1 << 20).unwrap();
        vm.debugger().set_error_stops(UNCAUGHT);
        STOPS.take();
        assert!(run_through(&fiber, &mut c, &[]).is_err());
        let stops = STOPS.take();
        assert_eq!(stops[0].line, 4, "its line is known");
        assert!(stops[0].locals.is_empty());
        assert_eq!(
            stops[0].stack.iter().map(|f| f.native).collect::<Vec<_>>(),
            [true, true]
        );
    }

    #[test]
    fn an_error_raised_stops_at_the_scripts_line_with_its_message() {
        let (vm, mut c) = faults("raise", true);
        c.vm = &raw const vm;
        let fiber = Fiber::new(1 << 20).unwrap();
        vm.debugger().set_error_stops(UNCAUGHT);
        STOPS.take();
        assert!(run_through(&fiber, &mut c, &[]).is_err());
        let stops = STOPS.take();
        // `error` is a C function: the stop is at its caller's line.
        assert_eq!(stops[0].line, 12);
        assert_eq!(
            stops[0].error,
            Some(ErrorStop {
                message: String::from("boom 1"),
                caught: false,
            })
        );
        assert_eq!(frames(&stops[0].stack), [(Some("raise"), Some(12))]);
    }

    #[test]
    fn an_error_a_pcall_catches_stops_only_when_asked() {
        let (vm, mut c) = faults("guarded", true);
        c.vm = &raw const vm;
        let fiber = Fiber::new(1 << 20).unwrap();
        let debugger = vm.debugger();
        // Errors not caught stop, but this one is caught: the script goes on.
        debugger.set_error_stops(UNCAUGHT);
        vm.set_debugging(true);
        STOPS.take();
        assert_eq!(run_through(&fiber, &mut c, &[]), Ok(2.0));
        assert!(STOPS.take().is_empty());
        debugger.set_error_stops(ErrorStops {
            uncaught: false,
            caught: true,
        });
        assert_eq!(run_through(&fiber, &mut c, &[Resume::Continue]), Ok(2.0));
        let stops = STOPS.take();
        assert_eq!(stops.len(), 1);
        assert_eq!(stops[0].line, 12);
        assert_eq!(
            stops[0].error,
            Some(ErrorStop {
                message: String::from("boom 1"),
                caught: true,
            })
        );
        assert_eq!(
            frames(&stops[0].stack),
            [
                (Some("raise"), Some(12)),
                (Some("pcall"), None),
                (Some("guarded"), Some(15))
            ]
        );
        // Once a line here too; a new Play stops again, and ended there the call ends at
        // its next interrupt.
        assert_eq!(run_through(&fiber, &mut c, &[]), Ok(2.0));
        assert!(STOPS.take().is_empty());
        debugger.reset_breakpoint_states();
        let r = run_through(&fiber, &mut c, &[Resume::Abort]);
        assert!(matches!(r, Err(ScriptError::Aborted { .. })), "{r:?}");
        debugger.end_abort();
    }

    #[test]
    fn the_budgets_error_stops_inside_the_loop() {
        let (vm, mut c) = faults("spin", true);
        c.vm = &raw const vm;
        let fiber = Fiber::new(1 << 20).unwrap();
        vm.debugger().set_error_stops(UNCAUGHT);
        STOPS.take();
        let r = run_through(&fiber, &mut c, &[Resume::Continue]);
        assert!(matches!(r, Err(ScriptError::Timeout { .. })), "{r:?}");
        let stops = STOPS.take();
        assert_eq!(stops.len(), 1);
        assert!((19..=22).contains(&stops[0].line), "{:?}", stops[0]);
        let e = stops[0].error.as_ref().unwrap();
        assert!(e.message.contains("time budget"), "{e:?}");
    }

    #[test]
    fn a_break_stops_at_the_next_line_a_hook_runs() {
        let (vm, mut c) = faults("tick", true);
        c.vm = &raw const vm;
        let fiber = Fiber::new(1 << 20).unwrap();
        let debugger = vm.debugger();
        // Hooks without a thread of their own cannot be broken into.
        assert!(!debugger.break_next());
        vm.set_debugging(true);
        assert!(debugger.break_next());
        assert!(debugger.break_pending());
        SEEN.take();
        STOPS.take();
        let r = run_through(&fiber, &mut c, &[Resume::StepOver, Resume::Continue]);
        assert!(r.is_err(), "the error, not stopped at: {r:?}");
        assert_eq!(SEEN.take(), [(8, true), (9, true)]);
        assert!(STOPS.take().iter().all(|s| s.error.is_none()));
        assert!(!debugger.break_pending(), "met");
        // Met once: the next call runs through.
        assert!(run_through(&fiber, &mut c, &[]).is_err());
        assert!(SEEN.take().is_empty());
    }
}
