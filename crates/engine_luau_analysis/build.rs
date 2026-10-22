//! Builds the vendored Luau Analysis (third_party/luau-analysis, Luau 0.736) as one static
//! library, and the C glue (`csrc/analysis.cpp`) apart. Analysis links against the Luau
//! libraries mlua-sys builds from the same release (luau0-src 0.21.0+luau736), so it sees
//! their defines (`LUA_API`, the vector size, the C stack's limit).
//!
//! Analysis is compiled with the same flags in every Cargo profile (always optimized), so
//! the library is built once per content hash (sources, headers, flags, compiler, target)
//! and shared by every profile, as Jolt's is (`engine_physics_jolt`). The cache lives in
//! `ENGINE_LUAU_ANALYSIS_CACHE` if set, else next to the target directory's profile
//! folders.

use std::io::Write as _;
use std::path::{Path, PathBuf};

fn collect(dir: &Path, out: &mut Vec<PathBuf>, extensions: &[&str]) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect(&path, out, extensions);
        } else if path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| extensions.contains(&e))
        {
            out.push(path);
        }
    }
}

/// FNV-1a over 64-bit words: stable across builds.
struct Hasher(u64);

impl Hasher {
    fn write(&mut self, bytes: &[u8]) {
        let mut chunks = bytes.chunks_exact(8);
        for c in &mut chunks {
            let mut w = [0u8; 8];
            w.copy_from_slice(c);
            self.0 = (self.0 ^ u64::from_le_bytes(w)).wrapping_mul(0x0000_0100_0000_01b3);
        }
        for &b in chunks.remainder() {
            self.0 = (self.0 ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01b3);
        }
        self.0 ^= bytes.len() as u64;
    }
}

/// `<target>/<profile>/build/<pkg>/out` → `<target>` (or `<target>/<triple>`).
fn target_dir(out_dir: &Path) -> Option<PathBuf> {
    out_dir.ancestors().nth(4).map(Path::to_path_buf)
}

/// The compiler setup of Analysis and the glue: Luau's headers, and mlua-sys's defines
/// for the Luau it builds (`find_vendored.rs`: a C stack of a million, vectors of 3).
fn configure(luau: &Path, msvc: bool) -> cc::Build {
    let mut build = cc::Build::new();
    build.cpp(true).std("c++17").warnings(false);
    for part in [
        "Analysis", "Ast", "Common", "Config", "Compiler", "Bytecode", "VM",
    ] {
        build.include(luau.join(part).join("include"));
    }
    build
        .define("LUA_API", "extern \"C\"")
        .define("LUA_VECTOR_SIZE", "3")
        .define("LUAI_MAXCSTACK", "1000000")
        .define("NDEBUG", None);
    if msvc {
        // Luau's analysis throws (its time limits, internal errors): the glue catches.
        build.flag("/EHsc").flag("/bigobj");
    }
    // Always optimized, as the rest of Luau is: a check runs as the developer types.
    build.opt_level(2).debug(false);
    build
}

fn main() {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let root = manifest.join("../../third_party");
    let luau = root.join("luau-analysis");
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed={}", luau.display());
    println!("cargo:rerun-if-changed=csrc");
    println!("cargo:rerun-if-env-changed=ENGINE_LUAU_ANALYSIS_CACHE");

    let target = std::env::var("TARGET").unwrap_or_default();
    let msvc = target.contains("msvc");

    // The glue: built apart, so changing it does not rebuild Analysis.
    let mut glue = configure(&luau, msvc);
    glue.file(manifest.join("csrc/analysis.cpp"));
    glue.compile("luau_analysis_glue");

    let mut sources = Vec::new();
    collect(&luau.join("Analysis/src"), &mut sources, &["cpp"]);
    sources.sort();
    let mut build = configure(&luau, msvc);
    build.files(&sources);

    // Content hash: every source and header, the compiler and its arguments, the target.
    let mut inputs = sources.clone();
    collect(&luau, &mut inputs, &["h"]);
    inputs.sort();
    inputs.dedup();
    let mut h = Hasher(0xcbf2_9ce4_8422_2325);
    for path in &inputs {
        h.write(
            path.strip_prefix(&root)
                .unwrap_or(path)
                .to_string_lossy()
                .as_bytes(),
        );
        h.write(&std::fs::read(path).unwrap_or_default());
    }
    let compiler = build.get_compiler();
    h.write(compiler.path().to_string_lossy().as_bytes());
    for arg in compiler.args() {
        h.write(arg.to_string_lossy().as_bytes());
    }
    h.write(target.as_bytes());
    let key = format!("{:016x}", h.0);

    let lib_name = if msvc {
        "luau_analysis.lib"
    } else {
        "libluau_analysis.a"
    };
    let out_dir = PathBuf::from(std::env::var("OUT_DIR").unwrap_or_default());
    let cache_root = std::env::var_os("ENGINE_LUAU_ANALYSIS_CACHE")
        .map(PathBuf::from)
        .or_else(|| target_dir(&out_dir).map(|t| t.join("luau-analysis-cache")));
    let cached = cache_root.as_ref().map(|r| r.join(&key));

    if let Some(dir) = &cached
        && dir.join(lib_name).is_file()
    {
        println!("cargo:rustc-link-search=native={}", dir.display());
        println!("cargo:rustc-link-lib=static=luau_analysis");
        return;
    }

    build.compile("luau_analysis");

    // Published for the other profiles: copied, then renamed into place, so a concurrent
    // build never links a half-written file.
    if let Some(dir) = cached
        && std::fs::create_dir_all(&dir).is_ok()
    {
        let temp = dir.join(format!("{lib_name}.{}.tmp", std::process::id()));
        if std::fs::copy(out_dir.join(lib_name), &temp).is_ok() {
            if std::fs::rename(&temp, dir.join(lib_name)).is_err() {
                let _ = std::fs::remove_file(&temp);
            }
            if let Ok(mut f) = std::fs::File::create(dir.join("SOURCE.txt")) {
                let _ = writeln!(
                    f,
                    "Luau 0.736 Analysis static library, key {key}, target {target}"
                );
            }
        }
    }
}
