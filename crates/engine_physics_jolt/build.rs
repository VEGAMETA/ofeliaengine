//! Builds the vendored Jolt Physics (third_party/JoltPhysics, v5.6.0) and JoltC
//! (third_party/joltc) as one static library. Both see the same defines, which must also
//! match what the Rust bindings assume (32-bit object layers, single precision).

use std::path::{Path, PathBuf};

fn collect(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect(&path, out);
        } else if path.extension().is_some_and(|e| e == "cpp") {
            out.push(path);
        }
    }
}

fn main() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../third_party");
    let jolt = root.join("JoltPhysics");
    let joltc = root.join("joltc");
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed={}", jolt.join("Jolt").display());
    println!("cargo:rerun-if-changed={}", joltc.display());

    let mut sources = Vec::new();
    collect(&jolt.join("Jolt"), &mut sources);
    sources.sort();
    sources.push(joltc.join("src/joltc.cpp"));
    sources.push(joltc.join("src/joltc_assert.cpp"));
    // Struct sizes and offsets for the binding layout tests.
    sources.push(Path::new(env!("CARGO_MANIFEST_DIR")).join("csrc/layout.cpp"));
    println!("cargo:rerun-if-changed=csrc");

    let target = std::env::var("TARGET").unwrap_or_default();
    let msvc = target.contains("msvc");
    let release = std::env::var("PROFILE").is_ok_and(|p| p != "debug");

    let mut build = cc::Build::new();
    build
        .cpp(true)
        .std("c++17")
        .files(&sources)
        .include(&jolt)
        .include(joltc.join("include"))
        .warnings(false)
        .define("JPH_OBJECT_LAYER_BITS", "32")
        .define("JPH_USE_SSE4_1", None)
        .define("JPH_USE_SSE4_2", None);
    if release {
        build.define("NDEBUG", None);
    }
    let cross_platform = std::env::var_os("CARGO_FEATURE_CROSS_PLATFORM_DETERMINISTIC").is_some();
    if cross_platform {
        build.define("JPH_CROSS_PLATFORM_DETERMINISTIC", None);
    }
    if msvc {
        // Jolt's own MSVC settings (Build/CMakeLists.txt): no RTTI, no exceptions, fast
        // floating point unless cross-platform determinism is requested.
        build
            .flag("/Zc:__cplusplus")
            .flag("/Zc:inline")
            .flag("/fp:except-")
            .flag("/GR-")
            .flag("/EHs-c-")
            .flag("/wd4577")
            .flag("/bigobj")
            .flag(if cross_platform {
                "/fp:precise"
            } else {
                "/fp:fast"
            });
    } else {
        build
            .flag("-fno-rtti")
            .flag("-fno-exceptions")
            .flag("-msse4.1")
            .flag("-msse4.2")
            .flag("-mpopcnt")
            .flag("-ffp-contract=off");
    }
    // Jolt is always optimized: debug builds of the engine would otherwise simulate an
    // order of magnitude slower (CLAUDE.md §15: dependencies at opt-level 3 in dev).
    build.opt_level(3).debug(false);
    build.compile("joltc");
}
