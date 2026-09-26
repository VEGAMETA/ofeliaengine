//! Luau scripting (CLAUDE.md §10, ADR 0032).
//!
//! [`ScriptVm`] is one sandboxed Luau VM:
//! - **Native code:** Luau's native code generator compiles loaded chunks to x64 when
//!   [`ScriptSettings::native`] is on (the default).
//! - **Limits:** every call into a script runs under a time budget (a runaway loop is
//!   stopped with [`ScriptError::Timeout`], the game keeps running) and the VM under a
//!   memory limit.
//! - **Modules and hot reload:** a module is a chunk that returns a table of functions. It
//!   receives a `state` table as its first argument; the same table is passed again when
//!   the module is reloaded, so state survives code changes. A reloaded module's
//!   `on_reload(old)` runs if present. If the new code does not compile, the old version
//!   keeps running and the error is returned with its line.
//! - **Errors** name the module and line ([`ScriptError`]), for the editor's "jump to the
//!   line".
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

use mlua::chunk::Compiler;
use mlua::{FromLuaMulti, IntoLuaMulti, Lua, Table, Value, VmState};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// VM options.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ScriptSettings {
    /// Largest heap in bytes before allocations fail.
    pub memory_limit: usize,
    /// Longest a single call into a script may run.
    pub call_budget: Duration,
    /// Compile loaded chunks to native code (Luau codegen).
    pub native: bool,
}

impl Default for ScriptSettings {
    fn default() -> Self {
        Self {
            memory_limit: 256 << 20,
            call_budget: Duration::from_millis(50),
            native: true,
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

struct Module {
    exports: Table,
    state: Table,
}

/// A sandboxed Luau VM with modules.
pub struct ScriptVm {
    lua: Lua,
    settings: ScriptSettings,
    /// Deadline of the running call, in nanoseconds since `epoch` (0: none).
    deadline: Arc<AtomicU64>,
    epoch: Instant,
    modules: HashMap<String, Module>,
}

impl ScriptVm {
    /// A new VM: sandboxed standard library, native codegen per `settings`.
    pub fn new(settings: ScriptSettings) -> Result<Self, ScriptError> {
        let lua = Lua::new();
        lua.set_compiler(
            Compiler::new()
                .set_optimization_level(2)
                .set_debug_level(1)
                .set_type_info_level(1),
        );
        lua.enable_jit(settings.native);
        lua.set_memory_limit(settings.memory_limit)
            .map_err(|_| ScriptError::OutOfMemory)?;
        let deadline = Arc::new(AtomicU64::new(0));
        let epoch = Instant::now();
        let d = Arc::clone(&deadline);
        lua.set_interrupt(move |_| {
            let limit = d.load(Ordering::Relaxed);
            if limit != 0 && epoch.elapsed().as_nanos() as u64 > limit {
                return Err(mlua::Error::runtime(TIMEOUT_MARKER));
            }
            Ok(VmState::Continue)
        });
        lua.sandbox(true).map_err(|e| ScriptError::Runtime {
            module: String::new(),
            line: None,
            message: e.to_string(),
        })?;
        Ok(Self {
            lua,
            settings,
            deadline,
            epoch,
            modules: HashMap::new(),
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
        self.arm();
        let result = function.call::<Value>(state.clone());
        self.disarm();
        let exports = match result.map_err(|e| self.map(name, e))? {
            Value::Table(t) => t,
            other => {
                return Err(ScriptError::Runtime {
                    module: name.to_owned(),
                    line: None,
                    message: format!("a module must return a table, not {}", other.type_name()),
                });
            }
        };
        let old = self.modules.insert(
            name.to_owned(),
            Module {
                exports: exports.clone(),
                state,
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
        let limit = self.epoch.elapsed() + self.settings.call_budget;
        self.deadline
            .store(limit.as_nanos().max(1) as u64, Ordering::Relaxed);
    }

    fn disarm(&self) {
        self.deadline.store(0, Ordering::Relaxed);
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
