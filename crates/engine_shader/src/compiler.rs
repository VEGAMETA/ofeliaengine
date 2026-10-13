//! One Slang session and the compile entry point.

use crate::ShaderError;
use crate::reflect::{StructLayout, natural_layout};
use crate::slang::{
    DEBUG_INFO_NONE, DEBUG_INFO_STANDARD, LANGUAGE_SLANG, LAYOUT_RULES_STRUCTURED_BUFFER,
    MATRIX_LAYOUT_COLUMN_MAJOR, OPTIMIZATION_HIGH, OPTIMIZATION_NONE, Request, Session,
    SlangLibrary, TARGET_SPIRV,
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

    fn from_raw(raw: u32) -> Option<Self> {
        Some(match raw {
            1 => Self::Vertex,
            5 => Self::Fragment,
            6 => Self::Compute,
            13 => Self::Mesh,
            14 => Self::Task,
            _ => return None,
        })
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
    ///
    /// With no `entries`, every function marked `[shader("<stage>")]` is compiled, in
    /// source order (Slang discovers them).
    pub fn compile_file(
        &mut self,
        path: &Path,
        entries: &[EntryPoint],
        options: &CompileOptions,
    ) -> Result<CompileOutput, ShaderError> {
        let source = std::fs::read_to_string(path)?;
        self.compile_source(path, &source, entries, options)
    }

    /// Compiles `source` (reported as `path` in diagnostics and dependencies). See
    /// [`Self::compile_file`] for what an empty `entries` means.
    pub fn compile_source(
        &mut self,
        path: &Path,
        source: &str,
        entries: &[EntryPoint],
        options: &CompileOptions,
    ) -> Result<CompileOutput, ShaderError> {
        engine_profiler::zone!("shader::Compiler::compile");
        let (request, entry_indices, diagnostics) =
            self.run(path, source, entries, options, false)?;
        let f = &self.library.functions;
        let req = request.raw.as_ptr();

        let targets: Vec<(EntryPoint, i32)> = if entries.is_empty() {
            // SAFETY: the request compiled successfully.
            unsafe { discovered_entry_points(f, req) }.map_err(|diagnostics| {
                ShaderError::Compile {
                    path: path.to_owned(),
                    diagnostics,
                }
            })?
        } else {
            entries.iter().cloned().zip(entry_indices).collect()
        };

        let mut compiled = Vec::with_capacity(targets.len());
        for (entry, index) in targets {
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
                name: entry.name,
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

    /// The layout of the struct `type_name` (for example `VelvetMaterial.Params`, an
    /// associated type) declared in `path` or what it includes, as shaders read it through
    /// a pointer ([`crate::reflect`]). Slang checks the source but generates no code
    /// (about a tenth of a second for `forward.slang`).
    pub fn reflect_struct(
        &mut self,
        path: &Path,
        options: &CompileOptions,
        type_name: &str,
    ) -> Result<StructLayout, ShaderError> {
        engine_profiler::zone!("shader::Compiler::reflect_struct");
        let source = std::fs::read_to_string(path)?;
        let (request, _, _) = self.run(path, &source, &[], options, true)?;
        let f = &self.library.functions;
        let error = |diagnostics: String| ShaderError::Compile {
            path: path.to_owned(),
            diagnostics,
        };
        let name = cstring(type_name);
        // SAFETY: the request compiled; every pointer comes from its reflection and is used
        // before the request is destroyed.
        let (struct_name, parts) = unsafe {
            let reflection = (f.reflection)(request.raw.as_ptr());
            if reflection.is_null() {
                return Err(error(String::from("Slang produced no reflection data")));
            }
            let ty = (f.find_type_by_name)(reflection, name.as_ptr());
            if ty.is_null() {
                return Err(error(format!("no type `{type_name}`")));
            }
            let layout = (f.type_layout)(reflection, ty, LAYOUT_RULES_STRUCTURED_BUFFER);
            if layout.is_null() {
                return Err(error(format!("no layout for `{type_name}`")));
            }
            let raw = (f.type_name)(ty);
            let struct_name = if raw.is_null() {
                type_name.to_owned()
            } else {
                CStr::from_ptr(raw).to_string_lossy().into_owned()
            };
            let parts =
                natural_layout(f, layout, "").map_err(|e| error(format!("`{type_name}`: {e}")))?;
            (struct_name, parts)
        };
        drop(request);
        Ok(StructLayout {
            name: struct_name,
            size: parts.size,
            fields: parts.fields,
        })
    }

    /// The entry points (`[shader("<stage>")]` functions) of `path`, in source order,
    /// without compiling them (the front end only).
    pub fn entry_points(
        &mut self,
        path: &Path,
        options: &CompileOptions,
    ) -> Result<Vec<EntryPoint>, ShaderError> {
        engine_profiler::zone!("shader::Compiler::entry_points");
        let source = std::fs::read_to_string(path)?;
        let (request, _, _) = self.run(path, &source, &[], options, true)?;
        // SAFETY: the request compiled.
        let found =
            unsafe { discovered_entry_points(&self.library.functions, request.raw.as_ptr()) }
                .map_err(|diagnostics| ShaderError::Compile {
                    path: path.to_owned(),
                    diagnostics,
                })?;
        Ok(found.into_iter().map(|(entry, _)| entry).collect())
    }

    /// Sets up and runs a compile request: the entry points' code, or with `check_only` the
    /// front end alone (reflection, no code). Returns the request, the entry points'
    /// indices and the diagnostics.
    fn run(
        &self,
        path: &Path,
        source: &str,
        entries: &[EntryPoint],
        options: &CompileOptions,
        check_only: bool,
    ) -> Result<(RequestGuard<'_>, Vec<i32>, String), ShaderError> {
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
        // Checking only: the front end and reflection, no code for any entry point.
        let args = [
            c"-fvk-use-entrypoint-name".as_ptr(),
            c"-skip-codegen".as_ptr(),
        ];
        let arg_count = if check_only { 2 } else { 1 };

        // SAFETY: every pointer passed below is a NUL-terminated string owned by this
        // frame and outliving the request; indices come from the calls that returned them.
        let (result, entry_indices) = unsafe {
            if (f.process_args)(req, args.as_ptr(), arg_count) < 0 {
                return Err(ShaderError::Compile {
                    path: path.to_owned(),
                    diagnostics: String::from("Slang rejected the compiler flags"),
                });
            }
            (f.set_codegen_target)(req, TARGET_SPIRV);
            // The engine's convention (ADR 0012): matrices are stored column by column, as
            // glam stores them, so they go into buffers and push constants unchanged.
            (f.set_matrix_layout)(req, MATRIX_LAYOUT_COLUMN_MAJOR);
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

        Ok((request, entry_indices, diagnostics))
    }
}

/// Entry points Slang found through `[shader("...")]` attributes, with their code indices.
///
/// # Safety
///
/// `req` must be a live request that compiled successfully.
unsafe fn discovered_entry_points(
    f: &crate::slang::Functions,
    req: *mut Request,
) -> Result<Vec<(EntryPoint, i32)>, String> {
    // SAFETY: guaranteed by the caller; the strings are copied before the request dies.
    unsafe {
        let reflection = (f.reflection)(req);
        if reflection.is_null() {
            return Err(String::from("Slang produced no reflection data"));
        }
        let count = (f.reflection_entry_point_count)(reflection);
        let mut found = Vec::with_capacity(count);
        for i in 0..count {
            let entry = (f.reflection_entry_point)(reflection, i);
            if entry.is_null() {
                continue;
            }
            let name_ptr = (f.entry_point_name)(entry);
            let name = if name_ptr.is_null() {
                String::new()
            } else {
                CStr::from_ptr(name_ptr).to_string_lossy().into_owned()
            };
            let raw_stage = (f.entry_point_stage)(entry);
            let stage = Stage::from_raw(raw_stage).ok_or_else(|| {
                format!(
                    "entry point `{name}` has a stage the engine does not support yet ({raw_stage})"
                )
            })?;
            found.push((EntryPoint { name, stage }, i as i32));
        }
        Ok(found)
    }
}

impl Drop for Compiler {
    fn drop(&mut self) {
        // SAFETY: created by `create_session`; no request is alive (they are scoped to
        // `compile_source`).
        unsafe { (self.library.functions.destroy_session)(self.session.as_ptr()) };
    }
}
