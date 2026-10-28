//! Luau scripting (CLAUDE.md §10, ADR 0032).
//!
//! [`ScriptVm`] is one sandboxed Luau VM:
//! - **Native code:** Luau's native code generator compiles loaded chunks to x64 when
//!   [`ScriptSettings::native`] is on (the default).
//! - **Limits:** every call into a script runs under a time budget (a runaway loop is
//!   stopped with [`ScriptError::Timeout`], the game keeps running) and the VM under a
//!   memory limit. The budget is Luau's own interrupt, set as a C function: it counts the
//!   interrupts (calls and loops' back edges) and reads the clock every
//!   [`CHECK_EVERY`] of them, so a loop's iteration pays an increment (ADR 0114 E7-1c).
//! - **Modules and hot reload:** a module is a chunk that returns a table of functions. It
//!   receives a `state` table as its first argument; the same table is passed again when
//!   the module is reloaded, so state survives code changes. A reloaded module's
//!   `on_reload(old)` runs if present. If the new code does not compile, the old version
//!   keeps running and the error is returned with its line.
//! - **Errors** name the module and line ([`ScriptError`]), for the editor's "jump to the
//!   line".
//! - **The debugger** ([`debug`], ADR 0119): breakpoints set on a module's lines, kept
//!   through its reloads; logpoints and pause points, with conditions and hit rules; stops
//!   at errors. Nothing is paid without them.
//!
//! ```
//! use engine_script::{ScriptSettings, ScriptVm};
//!
//! let mut vm = ScriptVm::new(ScriptSettings::default()).unwrap();
//! vm.load_module("counter", r#"
//!     local state = ...
//!     state.n = state.n or 0
//!     local M = {}
//!     function M.tick(step: number): number
//!         state.n += step
//!         return state.n
//!     end
//!     return M
//! "#).unwrap();
//! assert_eq!(vm.call::<_, f64>("counter", "tick", 2.0).unwrap(), 2.0);
//!
//! // Edit the code: the state is kept.
//! vm.load_module("counter", r#"
//!     local state = ...
//!     local M = {}
//!     function M.tick(step: number): number
//!         state.n += step * 10
//!         return state.n
//!     end
//!     return M
//! "#).unwrap();
//! assert_eq!(vm.call::<_, f64>("counter", "tick", 1.0).unwrap(), 12.0);
//! ```

pub use mlua;

pub mod debug;
pub use debug::{
    BreakAction, BreakState, Breakpoint, ErrorStop, ErrorStops, Frame, Hits, Logged, Place, Resume,
    ScriptDebugger, StopHandler, Stopped, Variable, with_instance,
};

use mlua::chunk::Compiler;
use mlua::{FromLuaMulti, Function, IntoLuaMulti, Lua, Table, Value};
use std::cell::Cell;
use std::collections::HashMap;
use std::ffi::c_int;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

/// Interrupts between two reads of the clock in a call under its budget.
pub const CHECK_EVERY: u32 = 64;

thread_local! {
    /// The deadline of the call running on this thread, in nanoseconds since
    /// [`clock_epoch`] (0: none): a VM never leaves its thread (mlua's `Lua` is not `Send`
    /// here), and calls into scripts do not nest.
    static DEADLINE: Cell<u64> = const { Cell::new(0) };
    /// Interrupts since the clock was last read.
    static SINCE_CHECK: Cell<u32> = const { Cell::new(0) };
}

/// What deadlines are counted from.
fn clock_epoch() -> Instant {
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    *EPOCH.get_or_init(Instant::now)
}

/// Nanoseconds since [`clock_epoch`].
fn now_ns() -> u64 {
    clock_epoch().elapsed().as_nanos() as u64
}

/// The running call's budget held while its script is stopped (ADR 0119): what was left
/// of it; none when no call runs under one.
pub(crate) fn hold_budget() -> Option<u64> {
    let deadline = DEADLINE.get();
    (deadline != 0).then(|| {
        DEADLINE.set(0);
        deadline.saturating_sub(now_ns())
    })
}

/// The running call's budget held, and one of `budget` from now instead (an evaluation at a
/// stop): give what it returns to [`release_budget`] after.
pub(crate) fn run_for(budget: Duration) -> Option<u64> {
    let held = hold_budget();
    DEADLINE.set(now_ns().saturating_add(budget.as_nanos() as u64).max(1));
    SINCE_CHECK.set(0);
    held
}

/// The budget [`hold_budget`] held, running again from now.
pub(crate) fn release_budget(left: Option<u64>) {
    match left {
        Some(left) => {
            DEADLINE.set(now_ns().saturating_add(left).max(1));
            SINCE_CHECK.set(0);
        }
        None => DEADLINE.set(0),
    }
}

/// Luau's interrupt, at calls and loops' back edges: a call past its deadline is stopped
/// with an error, the clock read every [`CHECK_EVERY`] interrupts.
///
/// # Safety
///
/// Luau calls it with its state; `gc` is -1 outside the collector's steps.
unsafe extern "C-unwind" fn interrupt(state: *mut mlua::ffi::lua_State, gc: c_int) {
    if gc >= 0 {
        return;
    }
    // Breakpoints reached since: placed again (`debug::rearm`), before the next lines run.
    if debug::rearm_pending() {
        // SAFETY: the running thread, between two instructions.
        unsafe { debug::rearm(state) };
    }
    // Ended at a stop that could not raise (an error a `pcall` caught): the call ends here.
    if debug::aborting() {
        // SAFETY: an interrupt may raise; nothing with a destructor is alive in this frame.
        unsafe {
            mlua::ffi::lua_pushlstring_(state, ABORT_MARKER.as_ptr().cast(), ABORT_MARKER.len());
            mlua::ffi::lua_error(state);
        }
    }
    let deadline = DEADLINE.get();
    if deadline == 0 {
        return;
    }
    let since = SINCE_CHECK.get() + 1;
    if since < CHECK_EVERY {
        SINCE_CHECK.set(since);
        return;
    }
    SINCE_CHECK.set(0);
    if now_ns() <= deadline {
        return;
    }
    // SAFETY: an interrupt may raise; nothing with a destructor is alive in this frame.
    unsafe {
        mlua::ffi::lua_pushlstring_(state, TIMEOUT_MARKER.as_ptr().cast(), TIMEOUT_MARKER.len());
        mlua::ffi::lua_error(state);
    }
}

/// VM options.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ScriptSettings {
    /// Largest heap in bytes before allocations fail.
    pub memory_limit: usize,
    /// Longest a single call into a script may run.
    pub call_budget: Duration,
    /// Compile loaded chunks to native code (Luau codegen).
    pub native: bool,
    /// Luau's debug level: 2 keeps locals' and upvalues' names, which the debugger's
    /// conditions and logpoints read (bytes of names, no cost per instruction); 1 keeps
    /// lines and functions' names, enough for errors' traces.
    pub debug_level: u8,
    /// Luau's optimization level: 2 inlines small local functions and unrolls loops; 1
    /// does neither, so a step goes into a function as its text reads (ADR 0119).
    pub optimization_level: u8,
}

impl Default for ScriptSettings {
    fn default() -> Self {
        Self {
            memory_limit: 256 << 20,
            call_budget: Duration::from_millis(50),
            native: true,
            debug_level: 2,
            optimization_level: 2,
        }
    }
}

/// What went wrong in a script.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScriptError {
    /// The code does not compile. The previous version (if any) keeps running.
    Compile {
        /// Module name.
        module: String,
        /// 1-based line, if known.
        line: Option<u32>,
        /// Compiler message.
        message: String,
    },
    /// A runtime error inside the script.
    Runtime {
        /// Module name.
        module: String,
        /// 1-based line of the innermost frame in this module, if known.
        line: Option<u32>,
        /// Error message.
        message: String,
    },
    /// The call ran past [`ScriptSettings::call_budget`] and was stopped.
    Timeout {
        /// Module name.
        module: String,
    },
    /// The developer ended the call at a stop ([`Resume::Abort`]).
    Aborted {
        /// Module name.
        module: String,
    },
    /// The script ran out of its memory limit.
    OutOfMemory,
    /// No module with this name is loaded.
    UnknownModule(String),
    /// The module has no function with this name.
    UnknownFunction {
        /// Module name.
        module: String,
        /// Function name.
        function: String,
    },
}

impl core::fmt::Display for ScriptError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let at = |line: &Option<u32>| line.map_or(String::new(), |l| format!(":{l}"));
        match self {
            Self::Compile {
                module,
                line,
                message,
            } => write!(f, "{module}{}: {message}", at(line)),
            Self::Runtime {
                module,
                line,
                message,
            } => write!(f, "{module}{}: {message}", at(line)),
            Self::Timeout { module } => write!(f, "{module}: script exceeded its time budget"),
            Self::Aborted { module } => write!(f, "{module}: {ABORT_MARKER}"),
            Self::OutOfMemory => write!(f, "scripts ran out of memory"),
            Self::UnknownModule(m) => write!(f, "no script module `{m}`"),
            Self::UnknownFunction { module, function } => {
                write!(f, "script module `{module}` has no function `{function}`")
            }
        }
    }
}

impl std::error::Error for ScriptError {}

const TIMEOUT_MARKER: &str = "script exceeded its time budget";
/// The error a call ended at a stop raises ([`ScriptError::Aborted`]).
pub(crate) const ABORT_MARKER: &str = "ended by the debugger at a stop";

struct Module {
    exports: Table,
    state: Table,
    /// The chunk that made it: what the debugger places breakpoints through again when a
    /// reload fails.
    chunk: Function,
}

/// A sandboxed Luau VM with modules.
pub struct ScriptVm {
    lua: Lua,
    settings: ScriptSettings,
    modules: HashMap<String, Module>,
    /// The thread hooks run on while debugging ([`Self::set_debugging`]), as a registry
    /// reference.
    hook_thread: Cell<Option<c_int>>,
    /// A hook runs on it, or with the debugger's message handler, now: a call into a script
    /// from inside one is made the plain way (an error stops once, in the outer hook).
    in_hook: Cell<bool>,
    /// The debugger's message handler ([`debug::error_handler`]), as a registry reference.
    error_handler: c_int,
}

impl ScriptVm {
    /// A new VM: sandboxed standard library, native codegen per `settings`.
    pub fn new(settings: ScriptSettings) -> Result<Self, ScriptError> {
        Self::new_with(settings, |_| Ok(()))
    }

    /// [`Self::new`], with `install` adding the host's globals (its libraries) before the
    /// sandbox makes the global table read-only.
    pub fn new_with(
        settings: ScriptSettings,
        install: impl FnOnce(&Lua) -> mlua::Result<()>,
    ) -> Result<Self, ScriptError> {
        let lua = Lua::new();
        lua.set_compiler(
            Compiler::new()
                .set_optimization_level(settings.optimization_level)
                .set_debug_level(settings.debug_level)
                .set_type_info_level(1),
        );
        lua.set_app_data(debug::Debugger::default());
        lua.enable_jit(settings.native);
        lua.set_memory_limit(settings.memory_limit)
            .map_err(|_| ScriptError::OutOfMemory)?;
        // SAFETY: the VM's state; the interrupt is a C function of this crate.
        unsafe {
            lua.exec_raw::<()>((), |state| {
                let callbacks = mlua::ffi::lua_callbacks(state);
                (*callbacks).interrupt = Some(interrupt);
                // Called only at a patched instruction: nothing without breakpoints.
                (*callbacks).debugbreak = Some(debug::debugbreak);
                // Called only while a thread single-steps (a step under way).
                (*callbacks).debugstep = Some(debug::debugstep);
                // Called only for an error a `pcall` caught.
                (*callbacks).debugprotectederror = Some(debug::protected_error);
            })
        }
        .map_err(|e| ScriptError::Runtime {
            module: String::new(),
            line: None,
            message: e.to_string(),
        })?;
        // SAFETY: the C function is kept in the registry, its reference the result.
        let error_handler = unsafe {
            lua.exec_raw::<c_int>((), |state| {
                mlua::ffi::lua_pushcfunction(state, debug::error_handler);
                let id = mlua::ffi::lua_ref(state, -1);
                mlua::ffi::lua_settop(state, 0);
                mlua::ffi::lua_pushinteger(state, id as _);
            })
        }
        .map_err(|e| ScriptError::Runtime {
            module: String::new(),
            line: None,
            message: e.to_string(),
        })?;
        let host = |e: mlua::Error| ScriptError::Runtime {
            module: String::new(),
            line: None,
            message: e.to_string(),
        };
        install(&lua).map_err(host)?;
        lua.sandbox(true).map_err(host)?;
        Ok(Self {
            lua,
            settings,
            modules: HashMap::new(),
            hook_thread: Cell::new(None),
            in_hook: Cell::new(false),
            error_handler,
        })
    }

    /// The underlying VM (for registering bindings).
    pub fn lua(&self) -> &Lua {
        &self.lua
    }

    /// Loads or reloads a module from source. On a reload the module keeps its `state`
    /// table and its `on_reload(old)` function is called; if the new code fails to
    /// compile or to run, the old version stays loaded.
    pub fn load_module(&mut self, name: &str, source: &str) -> Result<(), ScriptError> {
        engine_profiler::zone!("script::load_module");
        let state = match self.modules.get(name) {
            Some(m) => m.state.clone(),
            None => self.lua.create_table().map_err(|e| self.map(name, e))?,
        };
        // Each module gets its own global environment reading through to the (read-only)
        // standard library, so modules cannot leak globals into each other.
        let env = self.module_env().map_err(|e| self.map(name, e))?;
        let chunk = self
            .lua
            .load(source)
            .set_name(format!("={name}"))
            .set_environment(env);
        let function = chunk.into_function().map_err(|e| compile_error(name, &e))?;
        // Its breakpoints before it runs: its top level and the closures it makes have them.
        self.keep_chunk(name, &function);
        self.arm();
        let result = function.call::<Value>(state.clone());
        self.disarm();
        let exports = match result {
            Ok(Value::Table(t)) => t,
            failed => {
                // The old version stays, and the debugger goes back to its chunk.
                if let Some(old) = self.modules.get(name).map(|m| m.chunk.clone()) {
                    self.keep_chunk(name, &old);
                }
                return Err(match failed {
                    Err(e) => self.map(name, e),
                    Ok(other) => ScriptError::Runtime {
                        module: name.to_owned(),
                        line: None,
                        message: format!("a module must return a table, not {}", other.type_name()),
                    },
                });
            }
        };
        let old = self.modules.insert(
            name.to_owned(),
            Module {
                exports: exports.clone(),
                state,
                chunk: function,
            },
        );
        if let Some(old) = old
            && let Ok(mlua::Value::Function(hook)) = exports.get::<Value>("on_reload")
        {
            self.arm();
            let r = hook.call::<()>(old.exports);
            self.disarm();
            r.map_err(|e| self.map(name, e))?;
        }
        Ok(())
    }

    /// What module `name` returned, if it is loaded.
    pub fn exports(&self, name: &str) -> Option<&Table> {
        self.modules.get(name).map(|m| &m.exports)
    }

    /// Whether a module is loaded.
    pub fn has_module(&self, name: &str) -> bool {
        self.modules.contains_key(name)
    }

    /// Calls `module.function(args)` under the time budget.
    pub fn call<A: IntoLuaMulti, R: FromLuaMulti>(
        &mut self,
        module: &str,
        function: &str,
        args: A,
    ) -> Result<R, ScriptError> {
        if debug::aborting() {
            return Err(ScriptError::Aborted {
                module: module.to_owned(),
            });
        }
        let m = self
            .modules
            .get(module)
            .ok_or_else(|| ScriptError::UnknownModule(module.to_owned()))?;
        let f = match m.exports.get::<Value>(function) {
            Ok(Value::Function(f)) => f,
            _ => {
                return Err(ScriptError::UnknownFunction {
                    module: module.to_owned(),
                    function: function.to_owned(),
                });
            }
        };
        self.arm();
        let r = f.call::<R>(args);
        self.disarm();
        r.map_err(|e| self.map(module, e))
    }

    /// Calls `function`, a value of module `module` (a component's hook), with `args`
    /// under the time budget; its errors name `module`. While debugging it runs on the hook
    /// thread ([`Self::set_debugging`]).
    pub fn call_function<A: IntoLuaMulti, R: FromLuaMulti>(
        &self,
        module: &str,
        function: &Function,
        args: A,
    ) -> Result<R, ScriptError> {
        if debug::aborting() {
            return Err(ScriptError::Aborted {
                module: module.to_owned(),
            });
        }
        if !self.in_hook.get() {
            // On the hook thread only where a stop could be stepped from: a breakpoint that
            // stops is set, a step is under way, or a break asked (a resume costs 0.12 µs a
            // hook more).
            if let Some(thread) = self.hook_thread.get()
                && debug::thread_wanted()
            {
                return self.resume_hook(module, thread, function, args);
            }
            // Errors not caught stop: the plain call with the debugger's message handler,
            // which costs what mlua's own does.
            if debug::uncaught_wanted() {
                return self.call_handled(module, function, args);
            }
        }
        self.arm();
        let r = function.call::<R>(args);
        self.disarm();
        r.map_err(|e| self.map(module, e))
    }

    /// Debugging (ADR 0119 E9-1b-2): hooks run on a thread of their own from now on,
    /// resumed rather than called, so a stop in one can be stepped from; off, they are
    /// called the plain way again.
    pub fn set_debugging(&self, on: bool) {
        match (on, self.hook_thread.get()) {
            (true, None) => {
                // SAFETY: a thread made and kept in the registry; its reference is the
                // result, and the thread's pointer is the debugger's.
                let made = unsafe {
                    self.lua.exec_raw::<c_int>((), |state| {
                        let thread = mlua::ffi::lua_newthread(state);
                        if let Some(mut d) = self.lua.app_data_mut::<debug::Debugger>() {
                            d.set_hook_thread(core::ptr::NonNull::new(thread));
                        }
                        let id = mlua::ffi::lua_ref(state, -1);
                        mlua::ffi::lua_settop(state, 0);
                        mlua::ffi::lua_pushinteger(state, id as _);
                    })
                };
                if let Ok(id) = made {
                    self.hook_thread.set(Some(id));
                }
            }
            (false, Some(id)) => {
                if let Some(mut d) = self.lua.app_data_mut::<debug::Debugger>() {
                    d.set_hook_thread(None);
                }
                // SAFETY: the reference `set_debugging` made; nothing else holds it.
                let _ = unsafe {
                    self.lua.exec_raw::<()>((), |state| {
                        mlua::ffi::lua_unref(state, id);
                    })
                };
                self.hook_thread.set(None);
            }
            _ => {}
        }
    }

    /// [`Self::call_function`] on the hook thread (registry reference `thread`): resumed
    /// until it returns, through the breaks the debugger makes to enter or leave its
    /// stepping loop; its results moved back, an error raised again with its value.
    fn resume_hook<A: IntoLuaMulti, R: FromLuaMulti>(
        &self,
        module: &str,
        thread: c_int,
        function: &Function,
        args: A,
    ) -> Result<R, ScriptError> {
        use mlua::ffi;
        self.in_hook.set(true);
        debug::hook_called();
        self.arm();
        // SAFETY: the function and its arguments are the closure's stack; they move to the
        // hook thread (not running: no hook is), which runs to its end, and what it returns
        // (or its error's value) moves back. A failed thread is reset for the next hook.
        let r = unsafe {
            self.lua.exec_raw::<R>((function, args), |state| {
                let n = ffi::lua_gettop(state);
                ffi::lua_rawgeti_(state, ffi::LUA_REGISTRYINDEX, thread);
                let co = ffi::lua_tothread(state, -1);
                ffi::lua_settop(state, -2);
                ffi::lua_xmove(state, co, n);
                let mut status = ffi::lua_resume_(co, state, n - 1);
                while status == ffi::LUA_BREAK {
                    status = ffi::lua_resume_(co, state, 0);
                }
                if status == ffi::LUA_OK {
                    let results = ffi::lua_gettop(co);
                    ffi::lua_xmove(co, state, results);
                    return;
                }
                if status == ffi::LUA_YIELD {
                    ffi::lua_pushstring(state, c"a hook cannot yield".as_ptr());
                } else {
                    ffi::lua_xmove(co, state, 1);
                    // The thread's frames are as the error left them until it is reset: a
                    // stop there, the message read where functions can run.
                    if status == ffi::LUA_ERRRUN && debug::uncaught_wanted() {
                        let message = debug::error_message(state, -1);
                        if debug::error_stop(co, message, false) {
                            ffi::lua_settop(state, -2);
                            ffi::lua_pushlstring_(
                                state,
                                ABORT_MARKER.as_ptr().cast(),
                                ABORT_MARKER.len(),
                            );
                        }
                    }
                }
                ffi::lua_resetthread(co);
                ffi::lua_error(state);
            })
        };
        self.disarm();
        self.in_hook.set(false);
        r.map_err(|e| self.map(module, e))
    }

    /// [`Self::call_function`] the plain way with the debugger's message handler, which
    /// Luau calls where an error the hook does not catch was raised, before its frames
    /// unwind: a stop there ([`debug::error_handler`]). It is mlua's own call made through
    /// its raw interface (`exec_raw_lua`, hidden from its documentation; mlua is pinned),
    /// and the error is taken back as mlua takes it (the host's errors and panics as they
    /// were raised).
    fn call_handled<A: IntoLuaMulti, R: FromLuaMulti>(
        &self,
        module: &str,
        function: &Function,
        args: A,
    ) -> Result<R, ScriptError> {
        use mlua::ffi;
        self.in_hook.set(true);
        self.arm();
        let r = self.lua.exec_raw_lua(|raw| {
            let state = raw.state();
            /// The stack as it was, whatever happens.
            struct Top(*mut ffi::lua_State, c_int);
            impl Drop for Top {
                fn drop(&mut self) {
                    // SAFETY: the VM's state, its top put back where it was.
                    unsafe { ffi::lua_settop(self.0, self.1) };
                }
            }
            // SAFETY: the handler, the function and its arguments are pushed above the
            // stack's top, called with `lua_pcall`, and what it left read; the stack is put
            // back as it was.
            unsafe {
                let top = Top(state, ffi::lua_gettop(state));
                if ffi::lua_checkstack(state, 3) == 0 {
                    return Err(mlua::Error::StackError);
                }
                ffi::lua_rawgeti_(state, ffi::LUA_REGISTRYINDEX, self.error_handler);
                let handler = ffi::lua_gettop(state);
                raw.push(function)?;
                let n = args.push_into_stack_multi(raw)?;
                let status = ffi::lua_pcall(state, n, ffi::LUA_MULTRET, handler);
                if status != ffi::LUA_OK {
                    return Err(match raw.pop::<Value>()? {
                        Value::Error(e) => *e,
                        Value::String(s) if status == ffi::LUA_ERRMEM => {
                            mlua::Error::MemoryError(s.to_string_lossy())
                        }
                        Value::String(s) => mlua::Error::RuntimeError(s.to_string_lossy()),
                        other => mlua::Error::RuntimeError(format!(
                            "(error object is a {} value)",
                            other.type_name()
                        )),
                    });
                }
                let results = ffi::lua_gettop(state) - handler;
                let r = R::from_stack_multi(results, raw);
                drop(top);
                r
            }
        });
        self.disarm();
        self.in_hook.set(false);
        r.map_err(|e| self.map(module, e))
    }

    /// The debugger apart from the VM: what reads and sets breakpoints while a script is
    /// stopped and the host's stage holds the VM ([`ScriptDebugger`]).
    pub fn debugger(&self) -> ScriptDebugger {
        ScriptDebugger::new(&self.lua)
    }

    /// Module `module`'s breakpoints from now on (ADR 0119), replacing its others; placed
    /// at once when it is loaded, else when it loads. Kept through its reloads.
    pub fn set_breakpoints(&self, module: &str, breakpoints: Vec<Breakpoint>) {
        self.debugger().set_breakpoints(module, breakpoints);
    }

    /// How module `module`'s breakpoints are doing, in the order they were set: where they
    /// were placed, their hits, a logpoint's latest message, an error.
    pub fn breakpoint_states(&self, module: &str) -> Vec<BreakState> {
        self.debugger().breakpoint_states(module)
    }

    /// The logpoints' messages since the last call.
    pub fn take_logged(&self) -> Vec<Logged> {
        self.debugger().take_logged()
    }

    /// The module and line of the pause point reached since the last call, if one was:
    /// the game pauses after the tick it was reached in.
    pub fn take_pause(&self) -> Option<(String, u32)> {
        self.debugger().take_pause()
    }

    /// Whether a pause point was reached and its pause not taken yet ([`Self::take_pause`]):
    /// the ticks left in the frame are not run.
    pub fn pause_asked(&self) -> bool {
        self.debugger().pause_asked()
    }

    /// Every breakpoint's hits, latest message and error back to none, as a new Play
    /// starts; where they are placed is kept.
    pub fn reset_breakpoint_states(&self) {
        self.debugger().reset_breakpoint_states();
    }

    /// Raised whenever a breakpoint's state changes (placed, hit, a message, an error): the
    /// editor reads [`Self::breakpoint_states`] again only when it moved.
    pub fn breakpoints_version(&self) -> u64 {
        self.debugger().breakpoints_version()
    }

    /// What a stop calls from now on ([`StopHandler`]); without one, stops pause the game
    /// after their tick.
    pub fn set_stop_handler(&self, handler: Option<StopHandler>) {
        self.debugger().set_stop_handler(handler);
    }

    /// Module `module`'s chunk kept in the registry for the debugger, which places its
    /// breakpoints through it (at once, again after they were reached, and when the editor
    /// changes them); the old chunk's let go of.
    fn keep_chunk(&self, module: &str, chunk: &Function) {
        // SAFETY: the chunk is the one argument pushed; `lua_ref` keeps it in the registry
        // and leaves it there.
        let id = unsafe {
            self.lua.exec_raw::<c_int>(chunk.clone(), |state| {
                let id = mlua::ffi::lua_ref(state, -1);
                mlua::ffi::lua_settop(state, 0);
                mlua::ffi::lua_pushinteger(state, id as _);
            })
        };
        let Ok(id) = id else {
            return;
        };
        let old = self
            .lua
            .app_data_mut::<debug::Debugger>()
            .and_then(|mut d| d.set_chunk(module, id));
        if let Some(old) = old {
            // SAFETY: a reference this VM made; nothing else holds it.
            let _ = unsafe {
                self.lua.exec_raw::<()>((), |state| {
                    mlua::ffi::lua_unref(state, old);
                })
            };
        }
        debug::place(&self.lua, module, id);
    }

    /// Bytes the VM's heap uses.
    pub fn used_memory(&self) -> usize {
        self.lua.used_memory()
    }

    /// Settings the VM was created with.
    pub fn settings(&self) -> &ScriptSettings {
        &self.settings
    }

    fn module_env(&self) -> mlua::Result<Table> {
        let env = self.lua.create_table()?;
        let meta = self.lua.create_table()?;
        meta.set("__index", self.lua.globals())?;
        env.set_metatable(Some(meta))?;
        // Mark it a safe environment, as Luau's own `luaL_sandboxthread` does for its
        // per-thread proxy: without the flag Luau assumes builtins may be replaced and
        // turns off fast calls and import resolution (16x slower boids, ADR 0032).
        // SAFETY: the table is pushed as the only argument; `lua_setsafeenv` only sets a
        // flag on it.
        unsafe {
            self.lua.exec_raw::<()>(env.clone(), |state| {
                mlua::ffi::lua_setsafeenv(state, -1, 1);
            })?;
        }
        Ok(env)
    }

    fn arm(&self) {
        let budget = self.settings.call_budget.as_nanos() as u64;
        DEADLINE.set(now_ns().saturating_add(budget).max(1));
        SINCE_CHECK.set(0);
        // Breakpoints reached in the last call, whose instructions may have patched
        // themselves back, before this one runs (`debug::rearm`).
        if debug::rearm_pending() {
            // SAFETY: the VM's state between calls.
            let _ = unsafe { self.lua.exec_raw::<()>((), |state| debug::rearm(state)) };
        }
    }

    fn disarm(&self) {
        DEADLINE.set(0);
    }

    fn map(&self, module: &str, e: mlua::Error) -> ScriptError {
        runtime_error(module, &e)
    }
}

/// `name:12: message` → (12, message).
fn split_location<'a>(module: &str, text: &'a str) -> (Option<u32>, &'a str) {
    let prefix = format!("{module}:");
    let Some(pos) = text.find(&prefix) else {
        return (None, text);
    };
    let rest = &text[pos + prefix.len()..];
    let digits = rest.chars().take_while(char::is_ascii_digit).count();
    let line = rest[..digits].parse().ok();
    let message = rest[digits..].trim_start_matches(':').trim();
    (line, message)
}

fn compile_error(module: &str, e: &mlua::Error) -> ScriptError {
    let text = match e {
        mlua::Error::SyntaxError { message, .. } => message.clone(),
        other => other.to_string(),
    };
    let (line, message) = split_location(module, &text);
    ScriptError::Compile {
        module: module.to_owned(),
        line,
        message: message.to_owned(),
    }
}

fn runtime_error(module: &str, e: &mlua::Error) -> ScriptError {
    let root = innermost(e);
    if let mlua::Error::MemoryError(_) = root {
        return ScriptError::OutOfMemory;
    }
    let text = match root {
        mlua::Error::RuntimeError(m) => m.clone(),
        other => other.to_string(),
    };
    if text.contains(TIMEOUT_MARKER) {
        return ScriptError::Timeout {
            module: module.to_owned(),
        };
    }
    if text.contains(ABORT_MARKER) {
        return ScriptError::Aborted {
            module: module.to_owned(),
        };
    }
    let first = text.lines().next().unwrap_or("");
    let (line, message) = split_location(module, first);
    ScriptError::Runtime {
        module: module.to_owned(),
        line,
        message: message.to_owned(),
    }
}

fn innermost(e: &mlua::Error) -> &mlua::Error {
    match e {
        mlua::Error::CallbackError { cause, .. } => innermost(cause),
        mlua::Error::WithContext { cause, .. } => innermost(cause),
        other => other,
    }
}
