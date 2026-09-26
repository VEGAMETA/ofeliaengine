//! Builds the vendored Box2D v3.1.1 (third_party/box2d, MIT) and `csrc/shim.c` as one
//! static library, always optimized: Box2D is small (about 40 C files), so no cache is
//! needed, but debug-built physics would be too slow to use in `dev` builds.

// A build script has no caller to return errors to: a missing vendored tree must stop the
// build with its path.
#![allow(clippy::expect_used)]

use std::path::PathBuf;

fn main() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../third_party/box2d");
    let mut build = cc::Build::new();
    build
        .std("c17")
        .opt_level(3)
        .debug(false)
        .warnings(false)
        .include(root.join("include"))
        .include(root.join("src"))
        .file("csrc/shim.c");
    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() != Ok("msvc") {
        // Strict C17 hides `clock_gettime` (Box2D's timer) on the non-Windows hosts that
        // build the engine for checks.
        build.define("_POSIX_C_SOURCE", "199309L");
    }
    let mut sources: Vec<PathBuf> = std::fs::read_dir(root.join("src"))
        .expect("third_party/box2d/src")
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "c"))
        .collect();
    sources.sort();
    for s in &sources {
        build.file(s);
    }
    build.compile("box2d");
    println!("cargo:rerun-if-changed=csrc/shim.c");
    println!("cargo:rerun-if-changed={}", root.display());
}
