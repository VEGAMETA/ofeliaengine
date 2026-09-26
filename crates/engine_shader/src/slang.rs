//! Run-time binding to the Slang compiler library's C API.
//!
//! Uses the flat `sp*` functions (declared in `slang-deprecated.h` since 2025 but still
//! exported and ABI-stable) instead of the COM interfaces, which would need hand-written
//! vtables. The choice and the migration trigger are in ADR 0008.

use crate::ShaderError;
use core::ffi::{c_char, c_int, c_void};
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub(crate) type Session = c_void;
pub(crate) type Request = c_void;
pub(crate) type Reflection = c_void;
pub(crate) type ReflectionEntryPoint = c_void;

/// `SlangCompileTarget::SLANG_SPIRV`.
pub(crate) const TARGET_SPIRV: c_int = 6;
/// `SlangSourceLanguage::SLANG_SOURCE_LANGUAGE_SLANG`.
pub(crate) const LANGUAGE_SLANG: c_int = 1;
/// `SlangOptimizationLevel`: none / high.
pub(crate) const OPTIMIZATION_NONE: u32 = 0;
pub(crate) const OPTIMIZATION_HIGH: u32 = 2;
/// `SlangMatrixLayoutMode::SLANG_MATRIX_LAYOUT_COLUMN_MAJOR` (Slang's default is row-major).
pub(crate) const MATRIX_LAYOUT_COLUMN_MAJOR: u32 = 2;
/// `SlangDebugInfoLevel`: none / standard.
pub(crate) const DEBUG_INFO_NONE: u32 = 0;
pub(crate) const DEBUG_INFO_STANDARD: u32 = 2;

pub(crate) struct Functions {
    pub(crate) create_session: unsafe extern "C" fn(*const c_char) -> *mut Session,
    pub(crate) destroy_session: unsafe extern "C" fn(*mut Session),
    pub(crate) create_request: unsafe extern "C" fn(*mut Session) -> *mut Request,
    pub(crate) destroy_request: unsafe extern "C" fn(*mut Request),
    pub(crate) process_args: unsafe extern "C" fn(*mut Request, *const *const c_char, c_int) -> i32,
    pub(crate) set_codegen_target: unsafe extern "C" fn(*mut Request, c_int),
    pub(crate) find_profile: unsafe extern "C" fn(*mut Session, *const c_char) -> u32,
    pub(crate) set_target_profile: unsafe extern "C" fn(*mut Request, c_int, u32),
    pub(crate) set_optimization_level: unsafe extern "C" fn(*mut Request, u32),
    pub(crate) set_debug_info_level: unsafe extern "C" fn(*mut Request, u32),
    pub(crate) set_matrix_layout: unsafe extern "C" fn(*mut Request, u32),
    pub(crate) add_search_path: unsafe extern "C" fn(*mut Request, *const c_char),
    pub(crate) add_define: unsafe extern "C" fn(*mut Request, *const c_char, *const c_char),
    pub(crate) add_translation_unit:
        unsafe extern "C" fn(*mut Request, c_int, *const c_char) -> c_int,
    pub(crate) add_source_string:
        unsafe extern "C" fn(*mut Request, c_int, *const c_char, *const c_char),
    pub(crate) add_entry_point:
        unsafe extern "C" fn(*mut Request, c_int, *const c_char, u32) -> c_int,
    pub(crate) compile: unsafe extern "C" fn(*mut Request) -> i32,
    pub(crate) diagnostics: unsafe extern "C" fn(*mut Request) -> *const c_char,
    pub(crate) dependency_count: unsafe extern "C" fn(*mut Request) -> c_int,
    pub(crate) dependency_path: unsafe extern "C" fn(*mut Request, c_int) -> *const c_char,
    pub(crate) entry_point_code:
        unsafe extern "C" fn(*mut Request, c_int, *mut usize) -> *const c_void,
    pub(crate) reflection: unsafe extern "C" fn(*mut Request) -> *mut Reflection,
    /// `SlangUInt` is 64-bit on 64-bit targets.
    pub(crate) reflection_entry_point_count: unsafe extern "C" fn(*mut Reflection) -> usize,
    pub(crate) reflection_entry_point:
        unsafe extern "C" fn(*mut Reflection, usize) -> *mut ReflectionEntryPoint,
    pub(crate) entry_point_name: unsafe extern "C" fn(*mut ReflectionEntryPoint) -> *const c_char,
    pub(crate) entry_point_stage: unsafe extern "C" fn(*mut ReflectionEntryPoint) -> u32,
}

/// The loaded Slang compiler library.
pub struct SlangLibrary {
    pub(crate) functions: Functions,
    version: String,
    path: PathBuf,
    // Keeps the code the function pointers point into alive; declared last so it drops last.
    _library: libloading::Library,
}

impl core::fmt::Debug for SlangLibrary {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("SlangLibrary")
            .field("version", &self.version)
            .field("path", &self.path)
            .finish()
    }
}

fn candidates() -> Vec<PathBuf> {
    let mut list = Vec::new();
    if let Some(path) = std::env::var_os("ENGINE_SLANG_LIBRARY") {
        list.push(PathBuf::from(path));
    }
    let names: &[&str] = if cfg!(windows) {
        &["slang-compiler.dll", "slang.dll"]
    } else {
        &["libslang-compiler.so", "libslang.so"]
    };
    if let Some(sdk) = std::env::var_os("VULKAN_SDK") {
        let dir = PathBuf::from(sdk).join(if cfg!(windows) { "Bin" } else { "lib" });
        list.extend(names.iter().map(|n| dir.join(n)));
    }
    // Plain names: the OS search path (PATH on Windows, ld.so paths on Linux).
    list.extend(names.iter().map(PathBuf::from));
    list
}

impl SlangLibrary {
    /// Finds and loads the library: `ENGINE_SLANG_LIBRARY`, then the Vulkan SDK
    /// (`VULKAN_SDK`), then the OS search path.
    pub fn load() -> Result<Arc<Self>, ShaderError> {
        let mut tried = Vec::new();
        for path in candidates() {
            match Self::load_from(&path) {
                Ok(library) => return Ok(library),
                Err(ShaderError::MissingSymbol(s)) => return Err(ShaderError::MissingSymbol(s)),
                Err(_) => tried.push(path.display().to_string()),
            }
        }
        Err(ShaderError::LibraryNotFound(tried))
    }

    /// Loads the library from an explicit path.
    pub fn load_from(path: &Path) -> Result<Arc<Self>, ShaderError> {
        // SAFETY: loading a shared library runs its initializers; the Slang library's are
        // benign. The library stays loaded while `Self` lives.
        let library = unsafe { libloading::Library::new(path) }
            .map_err(|e| ShaderError::LibraryNotFound(vec![format!("{}: {e}", path.display())]))?;
        macro_rules! symbol {
            ($name:literal) => {{
                // SAFETY: the symbol's type matches the C declaration in
                // `slang-deprecated.h` / `slang.h`; the library outlives the pointer.
                let symbol = unsafe { library.get(concat!($name, "\0").as_bytes()) }
                    .map_err(|_| ShaderError::MissingSymbol($name.to_owned()))?;
                *symbol
            }};
        }
        let functions = Functions {
            create_session: symbol!("spCreateSession"),
            destroy_session: symbol!("spDestroySession"),
            create_request: symbol!("spCreateCompileRequest"),
            destroy_request: symbol!("spDestroyCompileRequest"),
            process_args: symbol!("spProcessCommandLineArguments"),
            set_codegen_target: symbol!("spSetCodeGenTarget"),
            find_profile: symbol!("spFindProfile"),
            set_target_profile: symbol!("spSetTargetProfile"),
            set_optimization_level: symbol!("spSetOptimizationLevel"),
            set_debug_info_level: symbol!("spSetDebugInfoLevel"),
            set_matrix_layout: symbol!("spSetMatrixLayoutMode"),
            add_search_path: symbol!("spAddSearchPath"),
            add_define: symbol!("spAddPreprocessorDefine"),
            add_translation_unit: symbol!("spAddTranslationUnit"),
            add_source_string: symbol!("spAddTranslationUnitSourceString"),
            add_entry_point: symbol!("spAddEntryPoint"),
            compile: symbol!("spCompile"),
            diagnostics: symbol!("spGetDiagnosticOutput"),
            dependency_count: symbol!("spGetDependencyFileCount"),
            dependency_path: symbol!("spGetDependencyFilePath"),
            entry_point_code: symbol!("spGetEntryPointCode"),
            reflection: symbol!("spGetReflection"),
            reflection_entry_point_count: symbol!("spReflection_getEntryPointCount"),
            reflection_entry_point: symbol!("spReflection_getEntryPointByIndex"),
            entry_point_name: symbol!("spReflectionEntryPoint_getName"),
            entry_point_stage: symbol!("spReflectionEntryPoint_getStage"),
        };
        let build_tag: unsafe extern "C" fn() -> *const c_char = symbol!("spGetBuildTagString");
        // SAFETY: returns a static NUL-terminated string.
        let version = unsafe { std::ffi::CStr::from_ptr(build_tag()) }
            .to_string_lossy()
            .into_owned();
        Ok(Arc::new(Self {
            functions,
            version,
            path: path.to_owned(),
            _library: library,
        }))
    }

    /// Slang version (build tag), for example `2026.12`. Part of every cache key.
    pub fn version(&self) -> &str {
        &self.version
    }

    /// Where the library was loaded from.
    pub fn path(&self) -> &Path {
        &self.path
    }
}
