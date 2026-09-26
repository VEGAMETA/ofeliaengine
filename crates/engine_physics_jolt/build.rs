//! Builds the vendored Jolt Physics (third_party/JoltPhysics, v5.6.0) and JoltC
//! (third_party/joltc) as one static library. Both see the same defines, which must also
//! match what the Rust bindings assume (32-bit object layers, single precision).
//!
//! Jolt is compiled with the same flags in every Cargo profile (always optimized), so the
//! library is built once per content hash (sources, headers, flags, compiler, target) and
//! shared: `dev`, `release`, `shipping`, `profiling`, clippy and tests all link the same
//! file instead of compiling 150 C++ files each. The cache lives in
//! `ENGINE_JOLTC_CACHE` if set (CI keeps it between runs), else next to the target
//! directory's profile folders.

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

/// FNV-1a over 64-bit words: stable across builds, fast enough for the 6 MB of sources
/// even in an unoptimized build script.
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

fn main() {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let root = manifest.join("../../third_party");
    let jolt = root.join("JoltPhysics");
    let joltc = root.join("joltc");
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed={}", jolt.join("Jolt").display());
    println!("cargo:rerun-if-changed={}", joltc.display());
    println!("cargo:rerun-if-changed=csrc");
    println!("cargo:rerun-if-env-changed=ENGINE_JOLTC_CACHE");

    let mut sources = Vec::new();
    collect(&jolt.join("Jolt"), &mut sources, &["cpp"]);
    sources.sort();
    sources.push(joltc.join("src/joltc.cpp"));
    sources.push(joltc.join("src/joltc_assert.cpp"));
    // Struct sizes and offsets for the binding layout tests.
    sources.push(manifest.join("csrc/layout.cpp"));

    let target = std::env::var("TARGET").unwrap_or_default();
    let msvc = target.contains("msvc");
    let cross_platform = std::env::var_os("CARGO_FEATURE_CROSS_PLATFORM_DETERMINISTIC").is_some();

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
        .define("JPH_USE_SSE4_2", None)
        // The same in every profile, so one build serves them all (Jolt's own asserts
        // are controlled by JPH_ENABLE_ASSERTS, which stays off).
        .define("NDEBUG", None);
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

    // Content hash: every source and header, the compiler and its arguments, the target.
    let mut inputs = sources.clone();
    collect(&jolt.join("Jolt"), &mut inputs, &["h", "inl"]);
    collect(&joltc.join("include"), &mut inputs, &["h"]);
    inputs.sort();
    inputs.dedup();
    let mut h = Hasher(0xcbf2_9ce4_8422_2325);
    for path in &inputs {
        h.write(path.strip_prefix(&root).unwrap_or(path).to_string_lossy().as_bytes());
        h.write(&std::fs::read(path).unwrap_or_default());
    }
    let compiler = build.get_compiler();
    h.write(compiler.path().to_string_lossy().as_bytes());
    for arg in compiler.args() {
        h.write(arg.to_string_lossy().as_bytes());
    }
    h.write(target.as_bytes());
    let key = format!("{:016x}", h.0);

    let lib_name = if msvc { "joltc.lib" } else { "libjoltc.a" };
    let out_dir = PathBuf::from(std::env::var("OUT_DIR").unwrap_or_default());
    let cache_root = std::env::var_os("ENGINE_JOLTC_CACHE")
        .map(PathBuf::from)
        .or_else(|| target_dir(&out_dir).map(|t| t.join("joltc-cache")));
    let cached = cache_root.as_ref().map(|r| r.join(&key));

    if let Some(dir) = &cached
        && dir.join(lib_name).is_file()
    {
        println!("cargo:rustc-link-search=native={}", dir.display());
        println!("cargo:rustc-link-lib=static=joltc");
        if !msvc {
            println!("cargo:rustc-link-lib=stdc++");
        }
        return;
    }

    build.compile("joltc");

    // Publish for the other profiles: copy, then rename into place, so a concurrent
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
                let _ = writeln!(f, "Jolt Physics + JoltC static library, key {key}, target {target}");
            }
        }
    }
}
