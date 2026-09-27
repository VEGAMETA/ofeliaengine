//! Slang compilation tests (see `common` for when they run).

#![allow(clippy::print_stdout, clippy::unwrap_used)]

mod common;

use common::{slang, temp_dir};

use engine_shader::{
    CacheOutcome, CompileOptions, Compiler, CompilerPool, EntryPoint, ShaderCache, ShaderError,
    Stage,
};
use std::path::{Path, PathBuf};
use std::time::Instant;

fn triangle_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../engine_rhi/shaders/triangle.slang")
}

fn entry_names(spirv: &[u32]) -> Vec<String> {
    assert_eq!(spirv[0], 0x0723_0203, "SPIR-V magic");
    let mut names = Vec::new();
    let mut i = 5;
    while i < spirv.len() {
        let (op, count) = (spirv[i] & 0xFFFF, (spirv[i] >> 16) as usize);
        if op == 15 {
            // OpEntryPoint: execution model, id, name (literal string)...
            let bytes: Vec<u8> = spirv[i + 3..i + count]
                .iter()
                .flat_map(|w| w.to_le_bytes())
                .collect();
            let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
            names.push(String::from_utf8_lossy(&bytes[..end]).into_owned());
        }
        i += count.max(1);
    }
    names
}

#[test]
fn compiles_the_bootstrap_triangle_and_matches_the_committed_spirv() {
    let Some(library) = slang() else { return };
    let start = Instant::now();
    let mut compiler = Compiler::new(library.clone()).unwrap();
    let session_time = start.elapsed();
    let entries = [
        EntryPoint::new("vertexMain", Stage::Vertex),
        EntryPoint::new("fragmentMain", Stage::Fragment),
    ];
    let options = CompileOptions {
        optimize: true,
        ..CompileOptions::default()
    };
    let start = Instant::now();
    let output = compiler
        .compile_file(&triangle_path(), &entries, &options)
        .unwrap();
    let first = start.elapsed();
    let start = Instant::now();
    let _ = compiler
        .compile_file(&triangle_path(), &entries, &options)
        .unwrap();
    let second = start.elapsed();
    println!("session: {session_time:?}, first compile: {first:?}, second compile: {second:?}");

    assert_eq!(output.entries.len(), 2);
    assert_eq!(entry_names(&output.entries[0].spirv), vec!["vertexMain"]);
    assert_eq!(entry_names(&output.entries[1].spirv), vec!["fragmentMain"]);
    assert!(
        output
            .dependencies
            .iter()
            .any(|d| d.ends_with("triangle.slang")),
        "{:?}",
        output.dependencies
    );

    // The committed bootstrap SPIR-V was produced by Slang 2026.18.3; with the same compiler it
    // must match the source exactly (no stale binaries in the repository).
    if library.version() == "2026.18.3" {
        let committed = |name: &str| -> Vec<u32> {
            let bytes = std::fs::read(
                Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("../engine_rhi/shaders")
                    .join(name),
            )
            .unwrap();
            bytes
                .chunks_exact(4)
                .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
                .collect()
        };
        assert_eq!(output.entries[0].spirv, committed("triangle.vert.spv"));
        assert_eq!(output.entries[1].spirv, committed("triangle.frag.spv"));
    } else {
        println!("not comparing with committed SPIR-V (made with Slang 2026.18.3)");
    }
}

#[test]
fn errors_point_at_file_and_line() {
    let Some(library) = slang() else { return };
    let dir = temp_dir("errors");
    let path = dir.join("broken.slang");
    std::fs::write(&path, "[shader(\"compute\")]\n[numthreads(1,1,1)]\nvoid main() {\n  int x = undefined_thing;\n}\n").unwrap();
    let mut compiler = Compiler::new(library).unwrap();
    let error = compiler
        .compile_file(
            &path,
            &[EntryPoint::new("main", Stage::Compute)],
            &CompileOptions::default(),
        )
        .unwrap_err();
    let ShaderError::Compile { diagnostics, .. } = error else {
        panic!("unexpected error: {error}")
    };
    println!("{diagnostics}");
    assert!(diagnostics.contains("broken.slang"), "{diagnostics}");
    assert!(
        diagnostics.contains("(4)") || diagnostics.contains(":4"),
        "line number missing: {diagnostics}"
    );
    assert!(diagnostics.contains("undefined_thing"), "{diagnostics}");
}

#[test]
fn defines_change_the_output() {
    let Some(library) = slang() else { return };
    let dir = temp_dir("defines");
    let path = dir.join("define.slang");
    std::fs::write(
        &path,
        "RWStructuredBuffer<uint> output;\n[shader(\"compute\")]\n[numthreads(1,1,1)]\nvoid main() {\n#if FANCY\n  output[0] = 7;\n#else\n  output[0] = 3;\n#endif\n}\n",
    )
    .unwrap();
    let mut compiler = Compiler::new(library).unwrap();
    let entries = [EntryPoint::new("main", Stage::Compute)];
    let plain = compiler
        .compile_file(&path, &entries, &CompileOptions::default())
        .unwrap();
    let fancy = compiler
        .compile_file(
            &path,
            &entries,
            &CompileOptions {
                defines: vec![("FANCY".to_owned(), "1".to_owned())],
                ..CompileOptions::default()
            },
        )
        .unwrap();
    assert_ne!(plain.entries[0].spirv, fancy.entries[0].spirv);
}

#[test]
fn cache_tracks_imported_modules() {
    let Some(library) = slang() else { return };
    let dir = temp_dir("cache");
    let cache = ShaderCache::new(dir.join("cache")).unwrap();
    let common = dir.join("common.slang");
    let main = dir.join("main.slang");
    std::fs::write(
        &common,
        "module common;\npublic uint value() { return 1; }\n",
    )
    .unwrap();
    std::fs::write(
        &main,
        "import common;\nRWStructuredBuffer<uint> output;\n[shader(\"compute\")]\n[numthreads(1,1,1)]\nvoid main() { output[0] = value(); }\n",
    )
    .unwrap();
    let compilers = CompilerPool::new(library);
    let entries = [EntryPoint::new("main", Stage::Compute)];
    let options = CompileOptions::default();

    let (first, outcome) = cache
        .compile(&compilers, &main, &entries, &options)
        .unwrap();
    assert_eq!(outcome, CacheOutcome::Miss);
    println!("dependencies: {:?}", first.dependencies);
    assert!(
        first
            .dependencies
            .iter()
            .any(|d| d.ends_with("common.slang")),
        "{:?}",
        first.dependencies
    );

    let (again, outcome) = cache
        .compile(&compilers, &main, &entries, &options)
        .unwrap();
    assert_eq!(outcome, CacheOutcome::Hit);
    assert_eq!(again.entries, first.entries);

    // Editing only the imported module invalidates the entry.
    std::fs::write(
        &common,
        "module common;\npublic uint value() { return 2; }\n",
    )
    .unwrap();
    let (changed, outcome) = cache
        .compile(&compilers, &main, &entries, &options)
        .unwrap();
    assert_eq!(outcome, CacheOutcome::Miss);
    assert_ne!(changed.entries[0].spirv, first.entries[0].spirv);

    // Different options are a different entry.
    let (_, outcome) = cache
        .compile(
            &compilers,
            &main,
            &entries,
            &CompileOptions {
                optimize: true,
                ..CompileOptions::default()
            },
        )
        .unwrap();
    assert_eq!(outcome, CacheOutcome::Miss);
    // Hits need no session: only the misses above created one.
    assert_eq!(compilers.idle_count(), 1);
}

#[test]
fn discovers_entry_points_when_none_are_requested() {
    let Some(library) = slang() else { return };
    let mut compiler = Compiler::new(library).unwrap();
    let output = compiler
        .compile_file(&triangle_path(), &[], &CompileOptions::default())
        .unwrap();
    let found: Vec<(&str, Stage)> = output
        .entries
        .iter()
        .map(|e| (e.name.as_str(), e.stage))
        .collect();
    assert_eq!(
        found,
        [
            ("vertexMain", Stage::Vertex),
            ("fragmentMain", Stage::Fragment)
        ]
    );
    for entry in &output.entries {
        assert_eq!(entry_names(&entry.spirv), [entry.name.as_str()]);
    }
}
