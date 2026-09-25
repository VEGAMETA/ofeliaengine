//! One Slang session and the compile entry point.

use crate::ShaderError;
use crate::slang::{
    DEBUG_INFO_NONE, DEBUG_INFO_STANDARD, LANGUAGE_SLANG, OPTIMIZATION_HIGH, OPTIMIZATION_NONE,
    Request, Session, SlangLibrary, TARGET_SPIRV,
};
use core::ffi::{CStr, c_char};
use core::ptr::NonNull;
use std::ffi::CString;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Pipeline stage of an entry point.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Stage {
    /// Vertex shader.
    Vertex,
    /// Fragment (pixel) shader.
    Fragment,
    /// Compute shader.
    Compute,
    /// Task (amplification) shader.
    Task,
    /// Mesh shader.
    Mesh,
}

impl Stage {
    /// Slang's `SlangStage` value.
    fn raw(self) -> u32 {
        match self {
            Self::Vertex => 1,
            Self::Fragment => 5,
            Self::Compute => 6,
            Self::Mesh => 13,
            Self::Task => 14, // SLANG_STAGE_AMPLIFICATION
        }
    }

    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Vertex => "vertex",
            Self::Fragment => "fragment",
            Self::Compute => "compute",
            Self::Task => "task",
            Self::Mesh => "mesh",
        }
    }

    pub(crate) fn from_name(name: &str) -> Option<Self> {
        Some(match name {
            "vertex" => Self::Vertex,
            "fragment" => Self::Fragment,
            "compute" => Self::Compute,
            "task" => Self::Task,
            "mesh" => Self::Mesh,
            _ => return None,
        })
    }
}

/// An entry point to compile.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct EntryPoint {
    /// Function name in the source; also the SPIR-V entry point name.
    pub name: String,
    /// Stage.
    pub stage: Stage,
}

impl EntryPoint {
    /// Shorthand constructor.
    pub fn new(name: &str, stage: Stage) -> Self {
        Self {
            name: name.to_owned(),
            stage,
        }
    }
}

/// Compilation options. Everything here is part of the cache key.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct CompileOptions {
    /// Optimize (`-O2`). Off for fastest iteration.
    pub optimize: bool,
    /// Emit debug info (source visible in RenderDoc/Nsight; CLAUDE.md §9).
    pub debug_info: bool,
    /// Preprocessor defines.
    pub defines: Vec<(String, String)>,
    /// Extra directories for `import` / `#include`. The source file's own directory is
    /// always searched.
    pub include_dirs: Vec<PathBuf>,
}

/// SPIR-V of one entry point.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompiledEntry {
    /// Entry point name.
    pub name: String,
    /// Stage.
    pub stage: Stage,
    /// SPIR-V words.
    pub spirv: Vec<u32>,
}

/// Result of a compilation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompileOutput {
    /// One entry per requested entry point, in request order.
    pub entries: Vec<CompiledEntry>,
    /// Every file the compilation read (the source itself and all imports/includes).
    pub dependencies: Vec<PathBuf>,
    /// Warnings (empty when clean).
    pub diagnostics: String,
}

/// A Slang session. Not `Sync`: use one compiler per thread.
pub struct Compiler {
    library: Arc<SlangLibrary>,
    session: NonNull<Session>,
    spirv_profile: u32,
}

// SAFETY: a Slang session may move between threads; it is used by one thread at a time
// (`Compiler` is not `Sync` and `compile` takes `&mut self`).
unsafe impl Send for Compiler {}

impl core::fmt::Debug for Compiler {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Compiler")
            .field("slang", &self.library.version())
            .finish()
    }
}

struct RequestGuard<'a> {
    library: &'a SlangLibrary,
    raw: NonNull<Request>,
}

impl Drop for RequestGuard<'_> {
    fn drop(&mut self) {
        // SAFETY: created by `create_request` and destroyed once.
        unsafe { (self.library.functions.destroy_request)(self.raw.as_ptr()) };
    }
}

fn cstring(s: &str) -> CString {
    CString::new(s.replace('\0', "")).unwrap_or_default()
}

impl Compiler {
    /// Creates a session (loads Slang's core module; hundreds of milliseconds).
    pub fn new(library: Arc<SlangLibrary>) -> Result<Self, ShaderError> {
        engine_profiler::zone!("shader::Compiler::new");
        // SAFETY: null is the documented default argument.
        let session =
            NonNull::new(unsafe { (library.functions.create_session)(core::ptr::null()) })
                .ok_or_else(|| {
                    ShaderError::MissingSymbol("spCreateSession returned null".to_owned())
                })?;
        // SAFETY: live session, NUL-terminated profile name.
        let spirv_profile =
            unsafe { (library.functions.find_profile)(session.as_ptr(), c"spirv_1_6".as_ptr()) };
        Ok(Self {
            library,
            session,
            spirv_profile,
        })
    }

    /// The library this compiler uses.
    pub fn library(&self) -> &Arc<SlangLibrary> {
        &self.library
    }

    /// Compiles `path` to SPIR-V 1.6 for each entry point.
    pub fn compile_file(
        &mut self,
        path: &Path,
        entries: &[EntryPoint],
        options: &CompileOptions,
    ) -> Result<CompileOutput, ShaderError> {
        let source = std::fs::read_to_string(path)?;
        self.compile_source(path, &source, entries, options)
    }

    /// Compiles `source` (reported as `path` in diagnostics and dependencies).
    pub fn compile_source(
        &mut self,
        path: &Path,
        source: &str,
        entries: &[EntryPoint],
        options: &CompileOptions,
    ) -> Result<CompileOutput, ShaderError> {
        engine_profiler::zone!("shader::Compiler::compile");
        let f = &self.library.functions;
        // SAFETY: live session.
        let raw = NonNull::new(unsafe { (f.create_request)(self.session.as_ptr()) }).ok_or_else(
            || ShaderError::MissingSymbol("spCreateCompileRequest returned null".to_owned()),
        )?;
        let request = RequestGuard {
            library: &self.library,
            raw,
        };
        let req = request.raw.as_ptr();

        let path_string = path.to_string_lossy().into_owned();
        let path_c = cstring(&path_string);
        let source_c = cstring(source);
        let mut search_dirs: Vec<CString> = Vec::new();
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            search_dirs.push(cstring(&parent.to_string_lossy()));
        }
        search_dirs.extend(
            options
                .include_dirs
                .iter()
                .map(|d| cstring(&d.to_string_lossy())),
        );
        let defines: Vec<(CString, CString)> = options
            .defines
            .iter()
            .map(|(k, v)| (cstring(k), cstring(v)))
            .collect();
        let names: Vec<CString> = entries.iter().map(|e| cstring(&e.name)).collect();
        // SPIR-V entry points keep their source names (Slang otherwise renames them "main").
        let args = [c"-fvk-use-entrypoint-name".as_ptr()];

        // SAFETY: every pointer passed below is a NUL-terminated string owned by this
        // frame and outliving the request; indices come from the calls that returned them.
        let (result, entry_indices) = unsafe {
            if (f.process_args)(req, args.as_ptr(), args.len() as i32) < 0 {
                return Err(ShaderError::Compile {
                    path: path.to_owned(),
                    diagnostics: String::from("Slang rejected the compiler flags"),
                });
            }
            (f.set_codegen_target)(req, TARGET_SPIRV);
            (f.set_target_profile)(req, 0, self.spirv_profile);
            (f.set_optimization_level)(
                req,
                if options.optimize {
                    OPTIMIZATION_HIGH
                } else {
                    OPTIMIZATION_NONE
                },
            );
            (f.set_debug_info_level)(
                req,
                if options.debug_info {
                    DEBUG_INFO_STANDARD
                } else {
                    DEBUG_INFO_NONE
                },
            );
            for dir in &search_dirs {
                (f.add_search_path)(req, dir.as_ptr());
            }
            for (key, value) in &defines {
                (f.add_define)(req, key.as_ptr(), value.as_ptr());
            }
            let unit = (f.add_translation_unit)(req, LANGUAGE_SLANG, c"main".as_ptr());
            (f.add_source_string)(req, unit, path_c.as_ptr(), source_c.as_ptr());
            let entry_indices: Vec<i32> = entries
                .iter()
                .zip(&names)
                .map(|(e, name)| (f.add_entry_point)(req, unit, name.as_ptr(), e.stage.raw()))
                .collect();
            ((f.compile)(req), entry_indices)
        };

        // SAFETY: the request is alive; the returned strings stay valid until it is
        // destroyed and are copied out immediately.
        let diagnostics = unsafe {
            let text = (f.diagnostics)(req);
            if text.is_null() {
                String::new()
            } else {
                CStr::from_ptr(text).to_string_lossy().into_owned()
            }
        };
        if result < 0 {
            return Err(ShaderError::Compile {
                path: path.to_owned(),
                diagnostics,
            });
        }

        let mut compiled = Vec::with_capacity(entries.len());
        for (entry, &index) in entries.iter().zip(&entry_indices) {
            let mut size = 0usize;
            // SAFETY: `index` was returned by `add_entry_point` for this request.
            let code = unsafe { (f.entry_point_code)(req, index, &mut size) };
            if code.is_null() || size == 0 || !size.is_multiple_of(4) {
                return Err(ShaderError::Compile {
                    path: path.to_owned(),
                    diagnostics: format!(
                        "{diagnostics}no SPIR-V produced for entry point `{}`",
                        entry.name
                    ),
                });
            }
            let mut spirv = vec![0u32; size / 4];
            // SAFETY: `code` points to `size` bytes owned by the request; copied into an
            // aligned buffer of the same size.
            unsafe {
                core::ptr::copy_nonoverlapping(
                    code.cast::<u8>(),
                    spirv.as_mut_ptr().cast::<u8>(),
                    size,
                )
            };
            compiled.push(CompiledEntry {
                name: entry.name.clone(),
                stage: entry.stage,
                spirv,
            });
        }

        // SAFETY: as for diagnostics.
        let dependencies = unsafe {
            let count = (f.dependency_count)(req).max(0);
            (0..count)
                .filter_map(|i| {
                    let p: *const c_char = (f.dependency_path)(req, i);
                    (!p.is_null())
                        .then(|| PathBuf::from(CStr::from_ptr(p).to_string_lossy().into_owned()))
                })
                .collect()
        };
        drop(request);

        Ok(CompileOutput {
            entries: compiled,
            dependencies,
            diagnostics,
        })
    }
}

impl Drop for Compiler {
    fn drop(&mut self) {
        // SAFETY: created by `create_session`; no request is alive (they are scoped to
        // `compile_source`).
        unsafe { (self.library.functions.destroy_session)(self.session.as_ptr()) };
    }
}
