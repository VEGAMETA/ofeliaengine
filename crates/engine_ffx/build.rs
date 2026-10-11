//! Builds the effects the engine uses from the vendored FidelityFX SDK
//! (third_party/FidelityFX-SDK, v1.1.4): the FSR 3.1 upscaler, Brixelizer and Brixelizer
//! GI, with the Vulkan backend and the engine's interfaces (csrc/) as one static library,
//! against the vendored Vulkan headers (third_party/Vulkan-Headers). One library for all
//! of them: the SDK's shader blob dispatcher compiles in the effects it is built with, and
//! two builds would define the backend twice (ADR 0090). Nothing is linked from the Vulkan
//! SDK: the backend's few direct Vulkan calls are defined in csrc/vulkan.cpp (ADR 0060).
//!
//! As for Jolt (crates/engine_physics_jolt/build.rs), the library is compiled with the same
//! flags in every Cargo profile and cached by content hash (sources, headers, flags,
//! compiler, target), so all profiles and tools link one build. The cache lives in
//! `ENGINE_FFX_CACHE` if set, else next to the target directory's profile folders.

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

/// FNV-1a over 64-bit words: stable across builds, fast in an unoptimized build script.
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
    let ffx = root.join("FidelityFX-SDK/sdk");
    let vulkan = root.join("Vulkan-Headers/include");
    let csrc = manifest.join("csrc");
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed={}", ffx.display());
    println!("cargo:rerun-if-changed={}", vulkan.display());
    println!("cargo:rerun-if-changed={}", csrc.display());
    println!("cargo:rerun-if-env-changed=ENGINE_FFX_CACHE");

    let sources = vec![
        ffx.join("src/shared/ffx_assert.cpp"),
        ffx.join("src/shared/ffx_breadcrumbs_list.cpp"),
        ffx.join("src/shared/ffx_message.cpp"),
        ffx.join("src/shared/ffx_object_management.cpp"),
        ffx.join("src/backends/shared/ffx_shader_blobs.cpp"),
        ffx.join("src/backends/vk/ffx_vk.cpp"),
        ffx.join("src/components/fsr3upscaler/ffx_fsr3upscaler.cpp"),
        ffx.join("src/components/brixelizer/ffx_brixelizer.cpp"),
        ffx.join("src/components/brixelizer/ffx_brixelizer_raw.cpp"),
        ffx.join("src/components/brixelizergi/ffx_brixelizergi.cpp"),
        csrc.join("vulkan.cpp"),
        csrc.join("fsr.cpp"),
        csrc.join("brixelizer.cpp"),
        csrc.join("blobs_fsr3upscaler.cpp"),
        csrc.join("blobs_brixelizer.cpp"),
        csrc.join("blobs_brixelizergi.cpp"),
    ];

    let target = std::env::var("TARGET").unwrap_or_default();
    let msvc = target.contains("msvc");
    let mut build = cc::Build::new();
    build
        .cpp(true)
        .std("c++17")
        .files(&sources)
        .include(ffx.join("include"))
        .include(ffx.join("src/shared"))
        .include(ffx.join("src/backends/shared"))
        .include(ffx.join("src/backends/shared/blob_accessors"))
        .include(ffx.join("src/components"))
        .include(&vulkan)
        .include(&csrc)
        .warnings(false)
        // The accessors of the engine's effects in ffx_shader_blobs.cpp.
        .define("FFX_FSR3UPSCALER", None)
        .define("FFX_BRIXELIZER", None)
        .define("FFX_BRIXELIZER_GI", None)
        // The SDK's own definitions (sdk/CMakeLists.txt).
        .define("_UNICODE", None)
        .define("UNICODE", None)
        .define("_DISABLE_CONSTEXPR_MUTEX_CONSTRUCTOR", None)
        // The same in every profile, so one build serves them all. The SDK's asserts
        // need _DEBUG and the debug C runtime, so they stay off.
        .define("NDEBUG", None)
        .opt_level(2)
        .debug(false);
    if msvc {
        build.flag("/Zc:__cplusplus").flag("/EHsc");
    }

    // Content hash: every source and header, the compiler and its arguments, the target.
    let mut inputs = sources.clone();
    collect(&ffx, &mut inputs, &["h", "cpp"]);
    collect(&vulkan, &mut inputs, &["h"]);
    collect(&csrc, &mut inputs, &["h", "inc", "cpp"]);
    inputs.sort();
    inputs.dedup();
    let mut h = Hasher(0xcbf2_9ce4_8422_2325);
    for path in &inputs {
        h.write(
            path.strip_prefix(manifest.join("../.."))
                .unwrap_or(path)
                .to_string_lossy()
                .replace('\\', "/")
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

    let lib_name = if msvc { "ffx.lib" } else { "libffx.a" };
    let out_dir = PathBuf::from(std::env::var("OUT_DIR").unwrap_or_default());
    let cache_root = std::env::var_os("ENGINE_FFX_CACHE")
        .map(PathBuf::from)
        .or_else(|| target_dir(&out_dir).map(|t| t.join("ffx-cache")));
    let cached = cache_root.as_ref().map(|r| r.join(&key));

    if let Some(dir) = &cached
        && dir.join(lib_name).is_file()
    {
        println!("cargo:rustc-link-search=native={}", dir.display());
        println!("cargo:rustc-link-lib=static=ffx");
        if !msvc {
            println!("cargo:rustc-link-lib=stdc++");
        }
        return;
    }

    build.compile("ffx");

    // Publish for the other profiles: copy, then rename into place, so a concurrent build
    // never links a half-written file.
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
                    "FidelityFX SDK FSR 3.1 upscaler, Brixelizer, Brixelizer GI, Vulkan backend, engine interfaces; key {key}, target {target}"
                );
            }
        }
    }
}
