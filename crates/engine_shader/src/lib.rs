//! Shader compilation (CLAUDE.md §9): Slang → SPIR-V, in process, with a cache.
//!
//! - [`SlangLibrary`] loads the Slang compiler library at run time (the Vulkan SDK's
//!   `slang-compiler.dll`/`slang.dll`, or `ENGINE_SLANG_LIBRARY`). Games ship precompiled
//!   SPIR-V and never load it; the editor and tools do.
//! - [`Compiler`] owns one Slang session. Creating a session loads Slang's core module
//!   (~260 ms on the container), a compile in an existing session takes ~14 ms, so each
//!   compiling thread keeps its own compiler (ADR 0008).
//! - [`ShaderCache`] stores SPIR-V by content: the key covers the compiler version, the
//!   options, the entry points and the content of the source **and every file it
//!   imported**, as reported by Slang.

mod cache;
mod compiler;
mod slang;

pub use cache::{CacheOutcome, ShaderCache};
pub use compiler::{CompileOptions, CompileOutput, CompiledEntry, Compiler, EntryPoint, Stage};
pub use slang::SlangLibrary;

use std::fmt;
use std::path::PathBuf;

/// Errors from shader compilation.
#[derive(Debug)]
pub enum ShaderError {
    /// No Slang library found. Lists every path tried.
    LibraryNotFound(Vec<String>),
    /// The library lacks a required function.
    MissingSymbol(String),
    /// Slang reported errors; `diagnostics` has file:line messages.
    Compile {
        /// Source file.
        path: PathBuf,
        /// Compiler output.
        diagnostics: String,
    },
    /// File system error.
    Io(std::io::Error),
}

impl fmt::Display for ShaderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::LibraryNotFound(tried) => write!(
                f,
                "Slang compiler library not found (install the Vulkan SDK or set ENGINE_SLANG_LIBRARY); tried: {}",
                tried.join(", ")
            ),
            Self::MissingSymbol(name) => write!(f, "Slang library lacks `{name}`"),
            Self::Compile { path, diagnostics } => write!(
                f,
                "{}: shader compilation failed:\n{diagnostics}",
                path.display()
            ),
            Self::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for ShaderError {}

impl From<std::io::Error> for ShaderError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}
