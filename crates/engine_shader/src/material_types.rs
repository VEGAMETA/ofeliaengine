//! How material types compile (ADR 0092): the engine's `forward.slang` with
//! `-D MATERIAL=<type> -D MATERIAL_FILE="<module>"`, so its entry points shade with the
//! type. Shared by the renderer (types compiled at run time), the cook (types compiled into
//! packs) and tools; `shader_dir` is the engine's shader folder (`engine_render`'s
//! `ENGINE_SHADER_DIR`).

use crate::{CompileOptions, Compiler, EntryPoint, ShaderError, Stage, StructLayout};
use std::path::Path;

/// A type's compiled entry points: name and SPIR-V.
pub type CompiledEntries = Vec<(String, Vec<u32>)>;

/// What a material type compiles with.
pub fn material_type_options(shader_dir: &Path, module: &Path, type_name: &str) -> CompileOptions {
    let module = module.to_string_lossy().replace('\\', "/");
    CompileOptions {
        optimize: true,
        debug_info: false,
        defines: vec![
            (String::from("MATERIAL"), type_name.to_owned()),
            (String::from("MATERIAL_FILE"), format!("\"{module}\"")),
        ],
        include_dirs: vec![shader_dir.to_owned()],
    }
}

/// The layout of a material type's parameters (`<type>.Params`) as its shaders read them,
/// through Slang's reflection: what a material file's values are laid out by. About a
/// tenth of a second (no code is generated).
pub fn reflect_material_params(
    compiler: &mut Compiler,
    shader_dir: &Path,
    module: &Path,
    type_name: &str,
) -> Result<StructLayout, ShaderError> {
    compiler.reflect_struct(
        &shader_dir.join("forward.slang"),
        &material_type_options(shader_dir, module, type_name),
        &format!("{type_name}.Params"),
    )
}

/// Every fragment entry point of `forward.slang` compiled for a material type, by name:
/// what a cooked pack carries so that a game never compiles a type. `compiler` lists them;
/// they compile on up to `threads` threads, each with a session of its own (one request per
/// thread: Slang shares the front end's work within a request).
pub fn compile_material_type(
    compiler: &mut Compiler,
    shader_dir: &Path,
    module: &Path,
    type_name: &str,
    threads: usize,
) -> Result<CompiledEntries, ShaderError> {
    compile_material_type_entries(compiler, shader_dir, module, type_name, threads, &|_| true)
}

/// [`compile_material_type`] for the fragment entry points `keep` accepts by name (a PSO
/// record's, ADR 0092).
pub fn compile_material_type_entries(
    compiler: &mut Compiler,
    shader_dir: &Path,
    module: &Path,
    type_name: &str,
    threads: usize,
    keep: &dyn Fn(&str) -> bool,
) -> Result<CompiledEntries, ShaderError> {
    let source = shader_dir.join("forward.slang");
    let options = material_type_options(shader_dir, module, type_name);
    let fragments: Vec<EntryPoint> = compiler
        .entry_points(&source, &options)?
        .into_iter()
        .filter(|e| e.stage == Stage::Fragment && keep(&e.name))
        .collect();
    if fragments.is_empty() {
        return Ok(Vec::new());
    }
    let threads = threads.clamp(1, fragments.len().max(1));
    if threads == 1 {
        let compiled = compiler.compile_file(&source, &fragments, &options)?;
        return Ok(compiled
            .entries
            .into_iter()
            .map(|e| (e.name, e.spirv))
            .collect());
    }
    let library = compiler.library().clone();
    let chunks: Vec<&[EntryPoint]> = fragments
        .chunks(fragments.len().div_ceil(threads))
        .collect();
    let results: Vec<Result<CompiledEntries, ShaderError>> = std::thread::scope(|scope| {
        let handles: Vec<_> = chunks
            .iter()
            .map(|chunk| {
                let (library, source, options) = (library.clone(), &source, &options);
                scope.spawn(move || {
                    let mut compiler = Compiler::new(library)?;
                    let compiled = compiler.compile_file(source, chunk, options)?;
                    Ok(compiled
                        .entries
                        .into_iter()
                        .map(|e| (e.name, e.spirv))
                        .collect())
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|h| {
                h.join().unwrap_or_else(|_| {
                    Err(ShaderError::MissingSymbol(String::from(
                        "a compile thread panicked",
                    )))
                })
            })
            .collect()
    });
    let mut out = Vec::with_capacity(fragments.len());
    for result in results {
        out.extend(result?);
    }
    Ok(out)
}
