//! The committed SPIR-V in `shaders/` must match its Slang source. Recompiles with the
//! Slang library when it is available and has the version the files were built with
//! (other versions produce different, equally valid code). Rebuild with:
//!
//! ```text
//! cargo run -p engine_cli -- shaders build -O --out crates/engine_render2d/shaders crates/engine_render2d/shaders
//! ```

#![allow(clippy::unwrap_used, clippy::print_stdout)]

use engine_shader::{CompileOptions, Compiler, SlangLibrary};
use std::path::Path;

/// Slang version that produced the committed files.
const BUILT_WITH: &str = "2026.18.3";

#[test]
fn committed_spirv_matches_the_source() {
    let library = match SlangLibrary::load() {
        Ok(library) => library,
        Err(e) if std::env::var("ENGINE_REQUIRE_SLANG").is_ok_and(|v| v == "1") => panic!("{e}"),
        Err(e) => {
            println!("skipping: {e}");
            return;
        }
    };
    if library.version() != BUILT_WITH {
        println!(
            "skipping: Slang {} differs from {BUILT_WITH}, which built the committed SPIR-V",
            library.version()
        );
        return;
    }
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("shaders");
    let mut compiler = Compiler::new(library).unwrap();
    let options = CompileOptions {
        optimize: true,
        ..CompileOptions::default()
    };
    let sources = engine_shader::find_shader_sources(std::slice::from_ref(&dir)).unwrap();
    assert!(!sources.is_empty(), "{sources:?}");
    for source in &sources {
        let stem = source.file_stem().unwrap().to_string_lossy().into_owned();
        let output = compiler.compile_file(source, &[], &options).unwrap();
        for entry in &output.entries {
            let file = dir.join(format!("{stem}.{}.spv", entry.name));
            let committed = std::fs::read(&file).unwrap();
            let fresh: Vec<u8> = entry.spirv.iter().flat_map(|w| w.to_le_bytes()).collect();
            assert!(
                committed == fresh,
                "{} is stale: rebuild the shaders (see this file's header)",
                file.display()
            );
            println!("{} is up to date", file.display());
        }
    }
}
