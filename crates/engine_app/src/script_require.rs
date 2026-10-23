//! `require` by string between the project's scripts (ADR 0114 §1, E7-5b): Luau's own
//! navigation (mlua's [`Require`]) over the modules the host loads, named by their paths
//! under `scripts/` (`npc/guard`), with the aliases of the `.luaurc` files above them.
//!
//! - **What a require returns:** what the module returned (a component's table, a table of
//!   functions), the one the host ran, cached by mlua's `require` per module.
//! - **Load order** ([`order`]): what a module requires runs before it. The paths of its
//!   `require` calls written as strings ([`requires`]) name those modules, and they load
//!   first, the rest in path order. A cycle is reported; its modules load in path order,
//!   and the require that finds its module not loaded yet is an error naming it.
//! - **Reloads:** a module run again drops its cached result and those of the modules that
//!   require it, which the host runs again after it (ADR 0114 §10).

use engine_script::mlua::{self, Function, Lua, Table, Value};
use mlua::luau::{NavigateError, Require};
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::PathBuf;
use std::rc::Rc;

/// The registry's table of every module's result, by name, which the loaders read.
const MODULES: &str = "ofelia::modules";
/// mlua's `require` cache (its own registry table), by module.
const CACHE: &str = "__MLUA_LOADER_CACHE";
/// A `.luaurc` file's name.
const LUAURC: &str = ".luaurc";

/// The project's modules as `require` sees them.
#[derive(Debug, Default)]
pub(crate) struct Modules {
    /// The project's folder, where `.luaurc` files are read.
    root: PathBuf,
    /// The scripts' folder, as path components from the project's (`["scripts"]`).
    folder: Vec<String>,
    /// The modules there.
    names: BTreeSet<String>,
}

impl Modules {
    /// The modules of folder `folder` (components from `root`), named so.
    pub(crate) fn new(
        root: PathBuf,
        folder: Vec<String>,
        names: impl IntoIterator<Item = String>,
    ) -> Self {
        Self {
            root,
            folder,
            names: names.into_iter().collect(),
        }
    }

    /// Module `name` is one of them from now on (a script made after the start).
    pub(crate) fn add(&mut self, name: &str) {
        self.names.insert(name.to_owned());
    }

    /// What path `at` (components from the project's folder) is.
    fn place(&self, at: &[String]) -> Place {
        let Some(inside) = at.strip_prefix(self.folder.as_slice()) else {
            // Above the scripts' folder, on the way to it: a folder.
            return if self.folder.starts_with(at) {
                Place::Folder
            } else {
                Place::Nothing
            };
        };
        let name = inside.join("/");
        if !name.is_empty()
            && inside.last().is_some_and(|l| l != "init")
            && self.names.contains(&name)
        {
            return Place::Module(name);
        }
        let init = if name.is_empty() {
            String::from("init")
        } else {
            format!("{name}/init")
        };
        if self.names.contains(&init) {
            return Place::Module(init);
        }
        let under = format!("{name}/");
        if name.is_empty() || self.names.iter().any(|n| n.starts_with(&under)) {
            return Place::Folder;
        }
        Place::Nothing
    }

    /// Module `from`'s path, components from the project's folder: its file's, or for an
    /// `init` its folder's (the folder is the module, as Luau has it).
    fn path_of(&self, from: &str) -> Vec<String> {
        let mut at = self.folder.clone();
        at.extend(from.split('/').map(str::to_owned));
        if at.last().is_some_and(|l| l == "init") {
            at.pop();
        }
        at
    }

    /// The aliases of the `.luaurc` in folder `at`, if there is one and it reads.
    fn aliases(&self, at: &[String]) -> Option<BTreeMap<String, String>> {
        let file = at
            .iter()
            .fold(self.root.clone(), |p, c| p.join(c))
            .join(LUAURC);
        let text = std::fs::read(&file).ok()?;
        let json: serde_json::Value = match serde_json::from_slice(&text) {
            Ok(json) => json,
            Err(e) => {
                log::warn!(target: "script", "{}: {e}", file.display());
                return None;
            }
        };
        let aliases = json.get("aliases")?.as_object()?;
        Some(
            aliases
                .iter()
                .filter_map(|(k, v)| Some((k.to_ascii_lowercase(), v.as_str()?.to_owned())))
                .collect(),
        )
    }

    /// The module `from` names by `path` (`./wallet`, `../shared/math`, `@lib/vector`),
    /// as Luau's navigation finds it.
    pub(crate) fn resolve(&self, from: &str, path: &str) -> Option<String> {
        let mut parts = path.split('/');
        let first = parts.next()?;
        let mut at = self.path_of(from);
        match first {
            // Beside the module: its path's parent (an init's folder's parent).
            "." => {
                at.pop()?;
            }
            ".." => {
                at.pop()?;
                at.pop()?;
            }
            alias if alias.starts_with('@') => {
                let alias = alias[1..].to_ascii_lowercase();
                // `@self` is the module's own path (an init's folder).
                if alias != "self" {
                    // The nearest `.luaurc` above the module naming it.
                    at.pop()?;
                    loop {
                        if let Some(target) = self.aliases(&at).and_then(|a| a.get(&alias).cloned())
                        {
                            for c in target.split('/') {
                                match c {
                                    "" | "." => {}
                                    ".." => {
                                        at.pop()?;
                                    }
                                    c => at.push(c.to_owned()),
                                }
                            }
                            break;
                        }
                        at.pop()?;
                    }
                }
            }
            _ => return None,
        }
        for c in parts {
            match c {
                "" | "." => {}
                ".." => {
                    at.pop()?;
                }
                c => at.push(c.to_owned()),
            }
        }
        match self.place(&at) {
            Place::Module(name) => Some(name),
            _ => None,
        }
    }
}

/// What a path is.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Place {
    Module(String),
    Folder,
    Nothing,
}

/// mlua's [`Require`] over [`Modules`].
struct ProjectRequire {
    modules: Rc<RefCell<Modules>>,
    /// Where navigation is: components from the project's folder.
    at: Vec<String>,
}

impl ProjectRequire {
    fn place(&self) -> Place {
        self.modules.borrow().place(&self.at)
    }

    fn found(&self) -> Result<(), NavigateError> {
        match self.place() {
            Place::Nothing => Err(NavigateError::NotFound),
            _ => Ok(()),
        }
    }
}

impl Require for ProjectRequire {
    fn is_require_allowed(&self, chunk_name: &str) -> bool {
        chunk_name
            .strip_prefix('=')
            .is_some_and(|m| self.modules.borrow().names.contains(m))
    }

    fn reset(&mut self, chunk_name: &str) -> Result<(), NavigateError> {
        let Some(module) = chunk_name.strip_prefix('=') else {
            return Err(NavigateError::NotFound);
        };
        self.at = self.modules.borrow().path_of(module);
        self.found()
    }

    fn jump_to_alias(&mut self, path: &str) -> Result<(), NavigateError> {
        // A path from the project's folder.
        self.at.clear();
        for c in path.split('/') {
            match c {
                "" | "." => {}
                ".." => {
                    self.at.pop().ok_or(NavigateError::NotFound)?;
                }
                c => self.at.push(c.to_owned()),
            }
        }
        self.found()
    }

    fn to_parent(&mut self) -> Result<(), NavigateError> {
        self.at.pop().ok_or(NavigateError::NotFound)?;
        self.found()
    }

    fn to_child(&mut self, name: &str) -> Result<(), NavigateError> {
        self.at.push(name.to_owned());
        self.found()
    }

    fn has_module(&self) -> bool {
        matches!(self.place(), Place::Module(_))
    }

    fn cache_key(&self) -> String {
        match self.place() {
            Place::Module(name) => name,
            _ => String::new(),
        }
    }

    fn has_config(&self) -> bool {
        let m = self.modules.borrow();
        self.at
            .iter()
            .fold(m.root.clone(), |p, c| p.join(c))
            .join(LUAURC)
            .is_file()
    }

    fn config(&self) -> std::io::Result<Vec<u8>> {
        let m = self.modules.borrow();
        std::fs::read(
            self.at
                .iter()
                .fold(m.root.clone(), |p, c| p.join(c))
                .join(LUAURC),
        )
    }

    fn loader(&self, lua: &Lua) -> mlua::Result<Function> {
        let Place::Module(name) = self.place() else {
            return Err(mlua::Error::runtime("require: not a module"));
        };
        lua.create_function(move |lua, ()| {
            let modules: Table = lua.named_registry_value(MODULES)?;
            match modules.raw_get::<Value>(name.as_str())? {
                Value::Nil => Err(mlua::Error::runtime(format!(
                    "require: `{name}` has not run yet (do two scripts require each other?)"
                ))),
                exports => Ok(exports),
            }
        })
    }
}

/// `require`, over `modules`, for the VM's globals.
pub(crate) fn require_function(lua: &Lua, modules: Rc<RefCell<Modules>>) -> mlua::Result<Function> {
    lua.set_named_registry_value(MODULES, lua.create_table()?)?;
    lua.create_require_function(ProjectRequire {
        modules,
        at: Vec::new(),
    })
}

/// Module `name` ran, returning `exports`: what `require` gives from now on.
pub(crate) fn ran(lua: &Lua, name: &str, exports: Table) -> mlua::Result<()> {
    let modules: Table = lua.named_registry_value(MODULES)?;
    modules.raw_set(name, exports)?;
    // A require made before is forgotten: the next one takes these.
    if let Ok(cache) = lua.named_registry_value::<Table>(CACHE) {
        cache.raw_set(name, Value::Nil)?;
    }
    Ok(())
}

/// The paths `source` requires by string (`require("./wallet")`, `require "./wallet"`), in
/// order, comments and other strings skipped.
pub(crate) fn requires(source: &str) -> Vec<String> {
    let b = source.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    // A long bracket's level at `at` (`[[` 0, `[==[` 2), if one opens there.
    let long = |at: usize| -> Option<(usize, usize)> {
        if b.get(at) != Some(&b'[') {
            return None;
        }
        let mut j = at + 1;
        while b.get(j) == Some(&b'=') {
            j += 1;
        }
        (b.get(j) == Some(&b'[')).then_some((j - at - 1, j + 1))
    };
    let close = |from: usize, level: usize| -> usize {
        let mut j = from;
        while j < b.len() {
            if b[j] == b']' {
                let mut k = j + 1;
                let mut n = 0;
                while b.get(k) == Some(&b'=') {
                    k += 1;
                    n += 1;
                }
                if n == level && b.get(k) == Some(&b']') {
                    return k + 1;
                }
            }
            j += 1;
        }
        b.len()
    };
    let quoted = |from: usize, q: u8| -> (usize, String) {
        let mut j = from;
        let mut text = Vec::new();
        while j < b.len() && b[j] != q && b[j] != b'\n' {
            if b[j] == b'\\' {
                j += 1;
            }
            if let Some(&c) = b.get(j) {
                text.push(c);
            }
            j += 1;
        }
        (j + 1, String::from_utf8_lossy(&text).into_owned())
    };
    while i < b.len() {
        match b[i] {
            b'-' if b.get(i + 1) == Some(&b'-') => {
                if let Some((level, start)) = long(i + 2) {
                    i = close(start, level);
                } else {
                    while i < b.len() && b[i] != b'\n' {
                        i += 1;
                    }
                }
            }
            b'"' | b'\'' | b'`' => i = quoted(i + 1, b[i]).0,
            b'[' if long(i).is_some() => {
                let (level, start) = long(i).unwrap_or((0, i + 1));
                i = close(start, level);
            }
            c if c.is_ascii_alphabetic() || c == b'_' => {
                let start = i;
                while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_') {
                    i += 1;
                }
                let field = start > 0 && matches!(b[start - 1], b'.' | b':');
                if &b[start..i] != b"require" || field {
                    continue;
                }
                let mut j = i;
                while j < b.len() && b[j].is_ascii_whitespace() {
                    j += 1;
                }
                if b.get(j) == Some(&b'(') {
                    j += 1;
                    while j < b.len() && b[j].is_ascii_whitespace() {
                        j += 1;
                    }
                }
                if let Some(&q) = b.get(j)
                    && (q == b'"' || q == b'\'')
                {
                    let (end, path) = quoted(j + 1, q);
                    out.push(path);
                    i = end;
                }
            }
            _ => i += 1,
        }
    }
    out
}

/// The order to load `modules` in (each name with what it requires, by name): what a
/// module requires before it, the rest in name order; and the cycles found, each as the
/// modules on it.
pub(crate) fn order(modules: &BTreeMap<String, Vec<String>>) -> (Vec<String>, Vec<Vec<String>>) {
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Mark {
        On,
        Done,
    }
    fn visit(
        name: &str,
        modules: &BTreeMap<String, Vec<String>>,
        marks: &mut HashMap<String, Mark>,
        stack: &mut Vec<String>,
        out: &mut Vec<String>,
        cycles: &mut Vec<Vec<String>>,
    ) {
        match marks.get(name) {
            Some(Mark::Done) => return,
            Some(Mark::On) => {
                if let Some(at) = stack.iter().position(|s| s == name) {
                    cycles.push(stack[at..].to_vec());
                }
                return;
            }
            None => {}
        }
        marks.insert(name.to_owned(), Mark::On);
        stack.push(name.to_owned());
        for dep in modules.get(name).into_iter().flatten() {
            if modules.contains_key(dep) {
                visit(dep, modules, marks, stack, out, cycles);
            }
        }
        stack.pop();
        marks.insert(name.to_owned(), Mark::Done);
        out.push(name.to_owned());
    }
    let mut marks = HashMap::new();
    let mut stack = Vec::new();
    let mut out = Vec::new();
    let mut cycles = Vec::new();
    for name in modules.keys() {
        visit(name, modules, &mut marks, &mut stack, &mut out, &mut cycles);
    }
    (out, cycles)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requires_are_found_by_their_strings_and_comments_and_strings_skipped() {
        let source = r#"--!strict
local Wallet = require("./wallet")
local M = require '../shared/math'
local V = require("@lib/vector")
-- require("./commented")
--[[ require("./long comment") ]]
local s = "require('./in a string')"
local t = [==[ require("./in a long string") ]==]
local u = thing.require("./a field")
local w = require( "./spaced" )
return Wallet
"#;
        assert_eq!(
            requires(source),
            ["./wallet", "../shared/math", "@lib/vector", "./spaced"]
        );
    }

    #[test]
    fn paths_resolve_as_luaus_navigation_does() {
        let m = Modules::new(
            PathBuf::from("/nowhere"),
            vec![String::from("scripts")],
            ["coin", "wallet", "npc/guard", "npc/init", "shared/math"].map(String::from),
        );
        assert_eq!(m.resolve("coin", "./wallet").as_deref(), Some("wallet"));
        assert_eq!(
            m.resolve("npc/guard", "../shared/math").as_deref(),
            Some("shared/math")
        );
        assert_eq!(
            m.resolve("npc/guard", "./init").as_deref(),
            None,
            "init is its folder's"
        );
        assert_eq!(m.resolve("coin", "./npc").as_deref(), Some("npc/init"));
        // An init's folder is the one above it.
        assert_eq!(m.resolve("npc/init", "./coin").as_deref(), Some("coin"));
        assert_eq!(
            m.resolve("npc/init", "@self/guard").as_deref(),
            Some("npc/guard")
        );
        assert_eq!(m.resolve("coin", "./nothing"), None);
        assert_eq!(
            m.resolve("coin", "wallet"),
            None,
            "a path starts with ./, ../ or @"
        );
    }

    #[test]
    fn modules_load_after_what_they_require_and_cycles_are_found() {
        let modules: BTreeMap<String, Vec<String>> = [
            ("coin", vec!["wallet"]),
            ("wallet", vec![]),
            ("a", vec!["b"]),
            ("b", vec!["a"]),
            ("zed", vec![]),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_owned(), v.into_iter().map(str::to_owned).collect()))
        .collect();
        let (order, cycles) = order(&modules);
        let at = |n: &str| order.iter().position(|o| o == n).unwrap();
        assert!(at("wallet") < at("coin"));
        assert_eq!(order.len(), 5);
        assert_eq!(cycles, vec![vec![String::from("a"), String::from("b")]]);
    }
}
