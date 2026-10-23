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
//!   - a **pause point** asks the game to pause after its tick (Rider's pausepoint).
//!
//!   Stops in the middle of a hook come with ADR 0119's answer (E9-1b).
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
}

/// What a breakpoint does when a script reaches it and its condition and hits allow.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BreakAction {
    /// Logs its text, `{expressions}` replaced by their values; the script goes on.
    Log(String),
    /// Asks the game to pause after the tick it is reached in.
    Pause,
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
    pause: bool,
}

impl Debugger {
    /// Module `module`'s breakpoints from now on, none placed yet; the old ones returned to
    /// be taken out.
    pub(crate) fn set(&mut self, module: &str, asked: Vec<Breakpoint>) -> Vec<u32> {
        let old = self.modules.insert(
            module.to_owned(),
            asked
                .into_iter()
                .map(|asked| Slot {
                    asked,
                    state: BreakState::default(),
                })
                .collect(),
        );
        self.forget_code(module);
        old.into_iter()
            .flatten()
            .filter_map(|s| s.state.placed)
            .collect()
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

    pub(crate) fn take_pause(&mut self) -> bool {
        std::mem::take(&mut self.pause)
    }
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
    if EVALUATING.get() {
        return;
    }
    // A panic never crosses Luau's frames: the breakpoint is passed over then.
    // SAFETY: as the caller says.
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe { reached(state) }));
}

/// [`debugbreak`]'s body.
///
/// # Safety
///
/// As [`debugbreak`]'s.
unsafe fn reached(state: *mut ffi::lua_State) {
    // SAFETY: as the caller says; `ar` is filled by `lua_getinfo` for the running frame,
    // its source a C string Luau keeps.
    unsafe {
        let mut ar: ffi::lua_Debug = core::mem::zeroed();
        if ffi::lua_getinfo(state, 0, c"sl".as_ptr(), &mut ar) == 0 || ar.source.is_null() {
            return;
        }
        let source = CStr::from_ptr(ar.source).to_string_lossy();
        let Some(module) = source.strip_prefix('=') else {
            return;
        };
        let Ok(line) = u32::try_from(ar.currentline) else {
            return;
        };
        let lua = Lua::get_or_init_from_ptr(state);
        let asks: Vec<Ask> = {
            let Some(mut dbg) = lua.app_data_mut::<Debugger>() else {
                return;
            };
            for id in dbg.dropped.drain(..) {
                ffi::lua_unref(state, id);
            }
            let Some(slots) = dbg.modules.get(module) else {
                return;
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
                    return;
                };
                let Some(s) = dbg
                    .modules
                    .get_mut(module)
                    .and_then(|m| m.get_mut(ask.slot))
                else {
                    continue;
                };
                s.state.hits += 1;
                s.asked.hits.admits(s.state.hits)
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
                BreakAction::Pause => {
                    if let Some(mut dbg) = lua.app_data_mut::<Debugger>() {
                        dbg.pause = true;
                    }
                }
            }
        }
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
    if let Some(mut dbg) = lua.app_data_mut::<Debugger>()
        && let Some(s) = dbg.modules.get_mut(module).and_then(|m| m.get_mut(slot))
    {
        s.state.error = Some(error);
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
    use crate::{BreakAction, Breakpoint, Hits, Logged, ScriptSettings, ScriptVm};

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
        assert!(!vm.take_pause());
        vm.call::<_, f64>("door", "tick", 5.0).unwrap();
        assert!(vm.take_pause(), "doubled 10");
        assert!(!vm.take_pause(), "taken");
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
}
