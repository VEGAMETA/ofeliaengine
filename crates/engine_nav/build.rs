//! Builds the vendored Recast and Detour (third_party/recastnavigation, Zlib) and
//! `csrc/shim.cpp` as one static library, always optimized: about 30 files, and a debug
//! build would make baking slow even in `dev` builds. Polygon references are 64-bit
//! (`DT_POLYREF64`): no limit on tiles times polygons per tile.

// A build script has no caller to return errors to: a missing vendored tree must stop the
// build with its path.
#![allow(clippy::expect_used)]

use std::path::PathBuf;

fn main() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../third_party/recastnavigation");
    let parts = ["Recast", "Detour", "DetourCrowd"];
    let mut build = cc::Build::new();
    build
        .cpp(true)
        .std("c++17")
        .opt_level(3)
        .debug(false)
        .warnings(false)
        .define("DT_POLYREF64", None)
        .file("csrc/shim.cpp");
    for part in parts {
        build.include(root.join(part).join("Include"));
        let mut sources: Vec<PathBuf> = std::fs::read_dir(root.join(part).join("Source"))
            .expect("third_party/recastnavigation sources")
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|e| e == "cpp"))
            .collect();
        sources.sort();
        for s in &sources {
            build.file(s);
        }
    }
    build.compile("recastnavigation");
    println!("cargo:rerun-if-changed=csrc/shim.cpp");
    println!("cargo:rerun-if-changed={}", root.display());
}
