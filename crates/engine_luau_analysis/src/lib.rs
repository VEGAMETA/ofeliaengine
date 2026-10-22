//! Luau's own analysis for the editor's code editor (ADR 0107 §6, E8-3): the type checker
//! and the linter of Luau 0.736 (MIT), the release `engine_script` runs, built from
//! `third_party/luau-analysis` and called in process.
//!
//! Modules are named by their path from the project's folder (`scripts/door.luau`). A
//! module set with [`Analysis::set_source`] is checked as it is in memory (a tab's
//! unsaved text); the others are read from the folder. `require("./door")` resolves by
//! string from the requiring module's folder, as Luau's require by string does.
//!
//! An [`Analysis`] keeps what it checked and checks again only what changed since. It is
//! used from one thread at a time (the editor's analysis worker, off the UI thread).
//!
//! ```no_run
//! use engine_luau_analysis::{Analysis, Severity, Solver};
//!
//! let Some(mut analysis) = Analysis::new(std::path::Path::new("my-game"), Solver::New) else {
//!     return;
//! };
//! analysis.set_source("scripts/door.luau", "--!strict\nlocal n: number = 'four'\n");
//! for d in analysis.check("scripts/door.luau") {
//!     assert_eq!(d.severity, Severity::Error);
//!     println!("{}:{}: {}", d.start.0 + 1, d.start.1 + 1, d.message);
//! }
//! ```

#![deny(unsafe_op_in_unsafe_fn)]

// The Luau libraries Analysis links against (Ast, Config, Compiler, VM) come with
// mlua-sys: linked though nothing of it is called from Rust.
extern crate mlua_sys as _;

use std::path::Path;
use std::ptr::NonNull;

mod ffi {
    #[repr(C)]
    pub(crate) struct EngineAnalysis {
        _private: [u8; 0],
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    pub(crate) struct EngineDiagnostic {
        pub(crate) line: u32,
        pub(crate) column: u32,
        pub(crate) end_line: u32,
        pub(crate) end_column: u32,
        pub(crate) severity: u32,
        pub(crate) message: *const u8,
        pub(crate) message_len: usize,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    pub(crate) struct EngineCompletion {
        pub(crate) label: *const u8,
        pub(crate) label_len: usize,
        pub(crate) kind: u32,
        pub(crate) flags: u32,
        pub(crate) detail: *const u8,
        pub(crate) detail_len: usize,
    }

    unsafe extern "C" {
        pub(crate) fn engine_analysis_complete(
            a: *mut EngineAnalysis,
            name: *const u8,
            name_len: usize,
            line: u32,
            column: u32,
            out: *mut EngineCompletion,
            cap: usize,
        ) -> usize;
        pub(crate) fn engine_analysis_hover(
            a: *mut EngineAnalysis,
            name: *const u8,
            name_len: usize,
            line: u32,
            column: u32,
            text: *mut *const u8,
        ) -> usize;
        pub(crate) fn engine_analysis_definition(
            a: *mut EngineAnalysis,
            name: *const u8,
            name_len: usize,
            line: u32,
            column: u32,
            module: *mut *const u8,
            module_len: *mut usize,
            out_line: *mut u32,
            out_column: *mut u32,
        ) -> i32;
        pub(crate) fn engine_analysis_signature(
            a: *mut EngineAnalysis,
            name: *const u8,
            name_len: usize,
            line: u32,
            column: u32,
            text: *mut *const u8,
            text_len: *mut usize,
            active_start: *mut u32,
            active_end: *mut u32,
        ) -> i32;
        pub(crate) fn engine_analysis_new(
            root: *const u8,
            root_len: usize,
            new_solver: i32,
        ) -> *mut EngineAnalysis;
        pub(crate) fn engine_analysis_free(a: *mut EngineAnalysis);
        pub(crate) fn engine_analysis_set_source(
            a: *mut EngineAnalysis,
            name: *const u8,
            name_len: usize,
            text: *const u8,
            text_len: usize,
        );
        pub(crate) fn engine_analysis_forget(
            a: *mut EngineAnalysis,
            name: *const u8,
            name_len: usize,
        );
        pub(crate) fn engine_analysis_add_definitions(
            a: *mut EngineAnalysis,
            name: *const u8,
            name_len: usize,
            text: *const u8,
            text_len: usize,
            error: *mut *const u8,
            error_len: *mut usize,
        ) -> usize;
        pub(crate) fn engine_analysis_check(
            a: *mut EngineAnalysis,
            name: *const u8,
            name_len: usize,
            out: *mut EngineDiagnostic,
            cap: usize,
        ) -> usize;
    }
}

/// Which of Luau's type solvers checks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Solver {
    /// The long-standing one.
    Old,
    /// The one Luau moves to, its own command line's default in 0.736 (`luau-analyze`):
    /// the editor's.
    New,
}

/// How much a finding matters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    /// A type error, a syntax error, a lint error.
    Error,
    /// A lint warning (an unused variable, a global shadowed).
    Warning,
}

/// What the analysis found in a module.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    /// Where it starts and ends: a line and a byte in it, from 0.
    pub start: (u32, u32),
    /// Where it ends.
    pub end: (u32, u32),
    /// How much it matters.
    pub severity: Severity,
    /// What Luau says (a lint warning's name first: `LocalUnused: …`).
    pub message: String,
}

/// What a completion is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompletionKind {
    /// A table's field or method.
    Property,
    /// A local, a global, a parameter.
    Binding,
    /// A keyword (`local`, `function`).
    Keyword,
    /// A string (`require`'s paths, a tagged function's).
    String,
    /// A type's name.
    Type,
    /// A module.
    Module,
    /// Something else Luau offers (a function written out, a hot comment).
    Other,
}

/// Something that may be written at a place.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Completion {
    /// What is written.
    pub label: String,
    /// What it is.
    pub kind: CompletionKind,
    /// Its type, as Luau writes it (empty for a keyword).
    pub detail: String,
    /// Of the type wanted there: offered first.
    pub fits: bool,
    /// A function: parentheses after it.
    pub call: bool,
    /// A function of no arguments: the caret after its parentheses, not between them.
    pub no_arguments: bool,
    /// Deprecated.
    pub deprecated: bool,
}

/// Luau's analysis over a project's modules.
pub struct Analysis {
    raw: NonNull<ffi::EngineAnalysis>,
    /// Room for a check's findings, kept between checks.
    found: Vec<ffi::EngineDiagnostic>,
}

// SAFETY: the analysis owns its Frontend and touches no thread-local state; it moves
// between threads whole and is never shared (no `Sync`).
unsafe impl Send for Analysis {}

impl Analysis {
    /// An analysis of the project in folder `root`, checking with `solver`; `None` when
    /// Luau could not make its frontend (out of memory).
    pub fn new(root: &Path, solver: Solver) -> Option<Self> {
        let root = root.to_string_lossy().replace('\\', "/");
        // SAFETY: the pointer and length name a live string for the call.
        let raw = unsafe {
            ffi::engine_analysis_new(root.as_ptr(), root.len(), i32::from(solver == Solver::New))
        };
        Some(Self {
            raw: NonNull::new(raw)?,
            found: Vec::new(),
        })
    }

    /// Module `name`'s text from now on: a tab's, unsaved changes and all.
    pub fn set_source(&mut self, name: &str, text: &str) {
        // SAFETY: `raw` is live, and the strings for the call.
        unsafe {
            ffi::engine_analysis_set_source(
                self.raw.as_ptr(),
                name.as_ptr(),
                name.len(),
                text.as_ptr(),
                text.len(),
            );
        }
    }

    /// Module `name` read from the project's folder again (its tab closed).
    pub fn forget_source(&mut self, name: &str) {
        // SAFETY: as above.
        unsafe { ffi::engine_analysis_forget(self.raw.as_ptr(), name.as_ptr(), name.len()) }
    }

    /// Loads type definitions (a `.d.luau`, the engine's API) into what every module sees.
    ///
    /// # Errors
    ///
    /// The first problem in them, with its line, and how many there are.
    pub fn add_definitions(&mut self, name: &str, text: &str) -> Result<(), (String, usize)> {
        let (mut error, mut error_len) = (core::ptr::null(), 0usize);
        // SAFETY: `raw` is live, the strings for the call, and the out-pointers point at
        // locals.
        let problems = unsafe {
            ffi::engine_analysis_add_definitions(
                self.raw.as_ptr(),
                name.as_ptr(),
                name.len(),
                text.as_ptr(),
                text.len(),
                &raw mut error,
                &raw mut error_len,
            )
        };
        if problems == 0 {
            return Ok(());
        }
        // SAFETY: on problems, `error` holds `error_len` bytes the analysis keeps until
        // its next call.
        let message = unsafe { text_of(error, error_len) };
        Err((message, problems))
    }

    /// Checks module `name` (with what it requires, again only where something changed):
    /// its type errors and lint findings, in Luau's order.
    pub fn check(&mut self, name: &str) -> Vec<Diagnostic> {
        let mut out = Vec::new();
        loop {
            let cap = self.found.capacity().max(64);
            self.found.clear();
            self.found.reserve(cap);
            // SAFETY: `raw` is live; `found` has room for `cap` records, of which the
            // call writes at most `cap` and reports how many there were.
            let n = unsafe {
                ffi::engine_analysis_check(
                    self.raw.as_ptr(),
                    name.as_ptr(),
                    name.len(),
                    self.found.as_mut_ptr(),
                    cap,
                )
            };
            if n > cap {
                self.found.reserve(n);
                // Checked again: the module is clean in Luau's cache, so this costs
                // only the copy.
                continue;
            }
            // SAFETY: the call wrote `n` records.
            unsafe { self.found.set_len(n) };
            for d in &self.found {
                out.push(Diagnostic {
                    start: (d.line, d.column),
                    end: (d.end_line, d.end_column),
                    severity: if d.severity == 0 {
                        Severity::Error
                    } else {
                        Severity::Warning
                    },
                    // SAFETY: each message is valid until the analysis is next called.
                    message: unsafe { text_of(d.message, d.message_len) },
                });
            }
            return out;
        }
    }
}

impl Analysis {
    /// What may be written at `line` and `column` (from 0, a byte in the line) of module
    /// `name`: those of the type wanted there first, then by kind and name.
    pub fn complete(&mut self, name: &str, line: u32, column: u32) -> Vec<Completion> {
        let mut found: Vec<ffi::EngineCompletion> = Vec::with_capacity(256);
        let mut out = Vec::new();
        loop {
            let cap = found.capacity();
            // SAFETY: `raw` is live; `found` has room for `cap` records, of which the
            // call writes at most `cap` and reports how many there were.
            let n = unsafe {
                ffi::engine_analysis_complete(
                    self.raw.as_ptr(),
                    name.as_ptr(),
                    name.len(),
                    line,
                    column,
                    found.as_mut_ptr(),
                    cap,
                )
            };
            if n > cap {
                found.reserve(n);
                continue;
            }
            // SAFETY: the call wrote `n` records.
            unsafe { found.set_len(n) };
            for c in &found {
                let kind = match c.kind {
                    0 => CompletionKind::Property,
                    1 => CompletionKind::Binding,
                    2 => CompletionKind::Keyword,
                    3 => CompletionKind::String,
                    4 => CompletionKind::Type,
                    5 => CompletionKind::Module,
                    _ => CompletionKind::Other,
                };
                out.push(Completion {
                    // SAFETY: the texts are valid until the analysis is next called.
                    label: unsafe { text_of(c.label, c.label_len) },
                    kind,
                    // SAFETY: as above.
                    detail: unsafe { text_of(c.detail, c.detail_len) },
                    fits: c.flags & 2 != 0,
                    call: c.flags & 4 != 0,
                    no_arguments: c.flags & 8 != 0,
                    deprecated: c.flags & 1 != 0,
                });
            }
            break;
        }
        let rank = |k: CompletionKind| match k {
            CompletionKind::Property | CompletionKind::Binding => 0,
            CompletionKind::Type | CompletionKind::Module | CompletionKind::String => 1,
            CompletionKind::Other => 2,
            CompletionKind::Keyword => 3,
        };
        out.sort_by(|a, b| {
            (!a.fits, a.deprecated, rank(a.kind), &a.label).cmp(&(
                !b.fits,
                b.deprecated,
                rank(b.kind),
                &b.label,
            ))
        });
        out
    }

    /// Where what is at `line` and `column` of module `name` is defined: its module (a
    /// path from the project's folder) and a line and a byte, from 0. A local's or a
    /// global's binding, a field where its table got it, a function where it was written;
    /// `None` for what Luau itself defines.
    pub fn definition(&mut self, name: &str, line: u32, column: u32) -> Option<(String, u32, u32)> {
        let (mut module, mut module_len) = (core::ptr::null(), 0usize);
        let (mut at_line, mut at_column) = (0u32, 0u32);
        // SAFETY: `raw` is live, the name for the call, the out-pointers point at locals.
        let found = unsafe {
            ffi::engine_analysis_definition(
                self.raw.as_ptr(),
                name.as_ptr(),
                name.len(),
                line,
                column,
                &raw mut module,
                &raw mut module_len,
                &raw mut at_line,
                &raw mut at_column,
            )
        };
        if found == 0 {
            return None;
        }
        // SAFETY: found: `module_len` bytes at `module`, valid until the next call.
        let module = unsafe { text_of(module, module_len) };
        Some((module, at_line, at_column))
    }

    /// The signature of the call whose parentheses `line` and `column` of module `name`
    /// are in (`double(n: number): number`), and the byte range in it of the argument
    /// the place is at (empty when the call has more arguments than the function).
    pub fn signature(
        &mut self,
        name: &str,
        line: u32,
        column: u32,
    ) -> Option<(String, (usize, usize))> {
        let (mut text, mut len) = (core::ptr::null(), 0usize);
        let (mut start, mut end) = (0u32, 0u32);
        // SAFETY: `raw` is live, the name for the call, the out-pointers point at locals.
        let found = unsafe {
            ffi::engine_analysis_signature(
                self.raw.as_ptr(),
                name.as_ptr(),
                name.len(),
                line,
                column,
                &raw mut text,
                &raw mut len,
                &raw mut start,
                &raw mut end,
            )
        };
        if found == 0 {
            return None;
        }
        // SAFETY: found: `len` bytes at `text`, valid until the next call.
        let text = unsafe { text_of(text, len) };
        Some((text, (start as usize, end as usize)))
    }

    /// What is at `line` and `column` of module `name`: its type, as Luau writes it.
    pub fn type_at(&mut self, name: &str, line: u32, column: u32) -> Option<String> {
        let mut text = core::ptr::null();
        // SAFETY: `raw` is live, the name for the call, and `text` a local.
        let n = unsafe {
            ffi::engine_analysis_hover(
                self.raw.as_ptr(),
                name.as_ptr(),
                name.len(),
                line,
                column,
                &raw mut text,
            )
        };
        // SAFETY: `n` bytes at `text`, valid until the analysis is next called.
        (n > 0).then(|| unsafe { text_of(text, n) })
    }
}

impl Drop for Analysis {
    fn drop(&mut self) {
        // SAFETY: `raw` came from `engine_analysis_new` and is freed once.
        unsafe { ffi::engine_analysis_free(self.raw.as_ptr()) }
    }
}

/// The UTF-8 text at `ptr`, `len` bytes (lossy where Luau's is not).
///
/// # Safety
///
/// `ptr` points at `len` readable bytes, or `len` is 0.
unsafe fn text_of(ptr: *const u8, len: usize) -> String {
    if ptr.is_null() || len == 0 {
        return String::new();
    }
    // SAFETY: the caller's.
    let bytes = unsafe { core::slice::from_raw_parts(ptr, len) };
    String::from_utf8_lossy(bytes).into_owned()
}

#[cfg(test)]
mod tests;
