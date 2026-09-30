//! Regenerates the FSR 3.1 upscaler's SPIR-V and its permutation tables from the vendored
//! FidelityFX SDK GLSL (ADR 0060). Run it after updating `third_party/FidelityFX-SDK`:
//!
//! `cargo run -p engine_fsr --example ffx_shaders [-- --glslang <glslangValidator.exe>]`
//!
//! It does what the SDK's CMake does with its prebuilt `FidelityFX_SC.exe` for the Vulkan
//! backend, for the permutations the engine uses only:
//!
//! - compiles every pass with glslang and the SDK's arguments
//!   (`sdk/src/backends/vk/CMakeShadersFSR3Upscaler.txt`,
//!   `sdk/include/FidelityFX/gpu/fsr3upscaler/CMakeCompileFSR3UpscalerShaders.txt`). The
//!   committed modules come from the glslang the SDK ships
//!   (`sdk/tools/binary_store/glslangValidator.exe` in the full SDK, 11.12.0), which
//!   reproduces `FidelityFX_SC.exe`'s modules byte for byte; without `--glslang` the Vulkan
//!   SDK's (`$VULKAN_SDK/Bin`) is used, which works but gives different bytes;
//! - keeps one copy of identical results;
//! - reads each module's descriptor bindings the way the SDK's compiler does with
//!   SPIRV-Reflect (name, binding, count, set per resource class, in binding order).
//!
//! One deviation from the SDK's modules, `FIXES`: storage images whose declared format
//! does not match the image the SDK binds to them get the image's format (the Vulkan
//! validation layer reports the mismatch; writes through it are undefined).
//!
//! Outputs, all committed: `shaders/*.spv`, `csrc/permutations.inc` (the reflection and
//! the permutation → module table the C++ blob accessor serves) and `src/blobs.rs` (the
//! modules, which Rust hands to the C++ side). The wave64 tables of the SDK are not
//! generated: on Vulkan they are the same modules (the backend requests the subgroup size),
//! and the Vulkan backend never asks for them (it reports shader model 5.1).

#![allow(
    missing_docs,
    clippy::print_stdout,
    clippy::print_stderr,
    clippy::unwrap_used,
    clippy::expect_used
)]

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Shader files, in the order of `ENGINE_FSR_SHADER_*` in `csrc/blobs.cpp`, and whether
/// the SDK's accessor has 16-bit variants of them (the others always load the 32-bit one).
const SHADERS: [(&str, bool); 10] = [
    ("prepare_inputs", false),
    ("luma_pyramid", false),
    ("shading_change_pyramid", false),
    ("shading_change", true),
    ("prepare_reactivity", true),
    ("luma_instability", false),
    ("accumulate", true),
    ("rcas", false),
    ("debug_view", false),
    ("autogen_reactive", true),
];

/// `FSR3UPSCALER_SHADER_PERMUTATION_*` bits (`ffx_fsr3upscaler_private.h`) and their
/// defines.
const OPTIONS: [(u32, &str); 6] = [
    (1 << 0, "FFX_FSR3UPSCALER_OPTION_REPROJECT_USE_LANCZOS_TYPE"),
    (1 << 1, "FFX_FSR3UPSCALER_OPTION_HDR_COLOR_INPUT"),
    (
        1 << 2,
        "FFX_FSR3UPSCALER_OPTION_LOW_RESOLUTION_MOTION_VECTORS",
    ),
    (1 << 3, "FFX_FSR3UPSCALER_OPTION_JITTERED_MOTION_VECTORS"),
    (1 << 4, "FFX_FSR3UPSCALER_OPTION_INVERTED_DEPTH"),
    (1 << 5, "FFX_FSR3UPSCALER_OPTION_APPLY_SHARPENING"),
];

/// Options fixed by how the engine creates the context (`src/lib.rs`): HDR color, motion
/// vectors at render resolution without jitter, depth 0 at the near plane.
const FIXED: u32 = (1 << 1) | (1 << 2);
/// Options that vary: the Lanczos lookup table (chosen by the device's subgroup sizes) and
/// sharpening (both accumulate pipelines are created).
const VARYING: [u32; 2] = [1 << 0, 1 << 5];

/// `FSR3UPSCALER_BASE_ARGS` and `FSR3UPSCALER_API_BASE_ARGS` without the SC-only ones.
const BASE_ARGS: [&str; 12] = [
    "-DFFX_GPU=1",
    "-DFFX_FSR3UPSCALER_OPTION_UPSAMPLE_SAMPLERS_USE_DATA_HALF=0",
    "-DFFX_FSR3UPSCALER_OPTION_ACCUMULATE_SAMPLERS_USE_DATA_HALF=0",
    "-DFFX_FSR3UPSCALER_OPTION_REPROJECT_SAMPLERS_USE_DATA_HALF=1",
    "-DFFX_FSR3UPSCALER_OPTION_POSTPROCESSLOCKSTATUS_SAMPLERS_USE_DATA_HALF=0",
    "-DFFX_FSR3UPSCALER_OPTION_UPSAMPLE_USE_LANCZOS_TYPE=2",
    "-e",
    "CS",
    "--target-env",
    "vulkan1.2",
    "-S",
    "comp",
];

/// SPIR-V `ImageFormat` values used below.
const FORMAT_RGBA16F: u32 = 2;
const FORMAT_RGBA8: u32 = 4;

/// Storage images whose declared format differs from the image the SDK binds: variable,
/// declared format, the bound image's format. `rw_luma_history`
/// (`ffx_fsr3upscaler_callbacks_glsl.h`) is declared `rgba8`; the SDK creates
/// `FSR3UPSCALER_LumaHistory1/2` as R16G16B16A16_FLOAT (`ffx_fsr3upscaler.cpp`).
const FIXES: [(&str, u32, u32); 1] = [("rw_luma_history", FORMAT_RGBA8, FORMAT_RGBA16F)];

#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct Resource {
    name: String,
    binding: u32,
    count: u32,
    set: u32,
}

/// Descriptor bindings by class, as the SDK's `FfxShaderBlob` has them.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct Reflection {
    cbv: Vec<Resource>,
    srv_texture: Vec<Resource>,
    uav_texture: Vec<Resource>,
    srv_buffer: Vec<Resource>,
    uav_buffer: Vec<Resource>,
    sampler: Vec<Resource>,
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let crate_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let sdk = crate_dir.join("../../third_party/FidelityFX-SDK/sdk");
    let glslang = args
        .iter()
        .position(|a| a == "--glslang")
        .and_then(|i| args.get(i + 1))
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("VULKAN_SDK")
                .map(|sdk| PathBuf::from(sdk).join("Bin").join("glslangValidator.exe"))
        })
        .expect("pass --glslang <path> or set VULKAN_SDK");
    let version = Command::new(&glslang).arg("--version").output().unwrap();
    let version = String::from_utf8_lossy(&version.stdout);
    let version = version
        .lines()
        .find(|l| l.starts_with("Glslang Version"))
        .unwrap_or("unknown")
        .trim()
        .to_owned();
    println!("glslang: {} ({version})", glslang.display());

    let gpu = sdk.join("include/FidelityFX/gpu");
    let temp = std::env::temp_dir().join(format!("engine_fsr_shaders_{}", std::process::id()));
    std::fs::create_dir_all(&temp).unwrap();

    // Every (shader, fp16, options) to compile.
    let mut jobs = Vec::new();
    for (shader, (name, has_fp16)) in SHADERS.iter().enumerate() {
        for fp16 in [false, true] {
            if fp16 && !has_fp16 {
                continue;
            }
            for combo in 0..(1u32 << VARYING.len()) {
                let options = VARYING
                    .iter()
                    .enumerate()
                    .filter(|(i, _)| combo & (1 << i) != 0)
                    .fold(FIXED, |acc, (_, bit)| acc | bit);
                jobs.push((shader, *name, fp16, options));
            }
        }
    }

    let results: Vec<(usize, bool, u32, Vec<u8>)> = std::thread::scope(|scope| {
        let handles: Vec<_> = jobs
            .iter()
            .map(|&(shader, name, fp16, options)| {
                let (glslang, gpu, sdk, temp) = (&glslang, &gpu, &sdk, &temp);
                scope.spawn(move || {
                    let source = sdk.join(format!(
                        "src/backends/vk/shaders/fsr3upscaler/ffx_fsr3upscaler_{name}_pass.glsl"
                    ));
                    let out = temp.join(format!("{name}_{}_{options}.spv", u8::from(fp16)));
                    let mut cmd = Command::new(glslang);
                    cmd.args(BASE_ARGS)
                        .arg("-Os")
                        .arg("-DFFX_GLSL=1")
                        .arg(format!("-DFFX_HALF={}", u8::from(fp16)));
                    for (bit, define) in OPTIONS {
                        cmd.arg(format!("-D{define}={}", u8::from(options & bit != 0)));
                    }
                    cmd.arg(format!("-I{}", gpu.display()))
                        .arg(format!("-I{}", gpu.join("fsr3upscaler").display()))
                        .arg("-o")
                        .arg(&out)
                        .arg(&source);
                    let output = cmd.output().unwrap();
                    if !output.status.success() {
                        panic!(
                            "{name} fp16={fp16} options={options:#x}:\n{}",
                            String::from_utf8_lossy(&output.stdout)
                        );
                    }
                    let mut spirv = std::fs::read(&out).unwrap();
                    fix_formats(&mut spirv);
                    (shader, fp16, options, spirv)
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    let _ = std::fs::remove_dir_all(&temp);

    // Unique modules, named by pass and content.
    let mut modules: Vec<(String, Vec<u8>, Reflection)> = Vec::new();
    let mut by_content: BTreeMap<Vec<u8>, usize> = BTreeMap::new();
    let mut permutations = Vec::new();
    for (shader, fp16, options, spirv) in results {
        let index = *by_content.entry(spirv.clone()).or_insert_with(|| {
            let reflection = reflect(&spirv);
            let name = format!("{}.{:016x}.spv", SHADERS[shader].0, fnv1a(&spirv));
            modules.push((name, spirv, reflection));
            modules.len() - 1
        });
        permutations.push((shader, fp16, options, index));
    }

    // Replace the previous outputs.
    let shaders_dir = crate_dir.join("shaders");
    std::fs::create_dir_all(&shaders_dir).unwrap();
    for entry in std::fs::read_dir(&shaders_dir).unwrap().flatten() {
        if entry.path().extension().is_some_and(|e| e == "spv") {
            std::fs::remove_file(entry.path()).unwrap();
        }
    }
    let mut total = 0;
    for (name, spirv, _) in &modules {
        std::fs::write(shaders_dir.join(name), spirv).unwrap();
        total += spirv.len();
    }

    let header = format!(
        "Generated by `cargo run -p engine_fsr --example ffx_shaders` from the FidelityFX SDK\n\
         GLSL in third_party/FidelityFX-SDK with {version}. Do not edit."
    );
    let mut inc = String::new();
    for line in header.lines() {
        writeln!(inc, "// {line}").unwrap();
    }
    writeln!(inc).unwrap();
    writeln!(inc, "#define ENGINE_FSR_MODULE_COUNT {}", modules.len()).unwrap();
    writeln!(inc).unwrap();
    let classes = |r: &Reflection| {
        [
            ("cbv", r.cbv.clone()),
            ("srv_texture", r.srv_texture.clone()),
            ("uav_texture", r.uav_texture.clone()),
            ("srv_buffer", r.srv_buffer.clone()),
            ("uav_buffer", r.uav_buffer.clone()),
            ("sampler", r.sampler.clone()),
        ]
    };
    for (i, (_, _, reflection)) in modules.iter().enumerate() {
        for (class, resources) in classes(reflection) {
            if resources.is_empty() {
                continue;
            }
            let list = |f: &dyn Fn(&Resource) -> String| {
                resources.iter().map(f).collect::<Vec<_>>().join(", ")
            };
            writeln!(
                inc,
                "static const char* m{i}_{class}_names[] = {{ {} }};",
                list(&|r| format!("\"{}\"", r.name))
            )
            .unwrap();
            for (field, get) in [
                (
                    "bindings",
                    &(|r: &Resource| r.binding) as &dyn Fn(&Resource) -> u32,
                ),
                ("counts", &|r: &Resource| r.count),
                ("spaces", &|r: &Resource| r.set),
            ] {
                writeln!(
                    inc,
                    "static const uint32_t m{i}_{class}_{field}[] = {{ {} }};",
                    list(&|r| get(r).to_string())
                )
                .unwrap();
            }
        }
    }
    writeln!(inc).unwrap();
    writeln!(
        inc,
        "static const EngineFsrReflection g_engine_fsr_reflection[ENGINE_FSR_MODULE_COUNT] = {{"
    )
    .unwrap();
    for (i, (name, _, reflection)) in modules.iter().enumerate() {
        let mut fields = Vec::new();
        for (class, resources) in classes(reflection) {
            if resources.is_empty() {
                fields.push("{ 0, nullptr, nullptr, nullptr, nullptr }".to_owned());
            } else {
                fields.push(format!(
                    "{{ {}, m{i}_{class}_names, m{i}_{class}_bindings, m{i}_{class}_counts, m{i}_{class}_spaces }}",
                    resources.len()
                ));
            }
        }
        writeln!(inc, "    // {i}: {name}").unwrap();
        writeln!(inc, "    {{ {} }},", fields.join(", ")).unwrap();
    }
    writeln!(inc, "}};").unwrap();
    writeln!(inc).unwrap();
    writeln!(
        inc,
        "// Shader (ENGINE_FSR_SHADER_*), 16-bit, FSR3UPSCALER_SHADER_PERMUTATION_* option bits, module."
    )
    .unwrap();
    writeln!(
        inc,
        "static const EngineFsrPermutation g_engine_fsr_permutations[] = {{"
    )
    .unwrap();
    for (shader, fp16, options, index) in &permutations {
        writeln!(
            inc,
            "    {{ {shader}, {}, 0x{options:02x}, {index} }},",
            u8::from(*fp16)
        )
        .unwrap();
    }
    writeln!(inc, "}};").unwrap();
    std::fs::write(crate_dir.join("csrc/permutations.inc"), inc).unwrap();

    let mut rs = String::new();
    for line in header.lines() {
        writeln!(rs, "// {line}").unwrap();
    }
    writeln!(rs).unwrap();
    writeln!(
        rs,
        "/// The SPIR-V modules `csrc/permutations.inc` refers to by index, 4-byte aligned."
    )
    .unwrap();
    writeln!(
        rs,
        "pub(crate) static MODULES: [&[u8]; {}] = [",
        modules.len()
    )
    .unwrap();
    for (name, _, _) in &modules {
        writeln!(rs, "    spirv!(\"../shaders/{name}\"),").unwrap();
    }
    writeln!(rs, "];").unwrap();
    std::fs::write(crate_dir.join("src/blobs.rs"), rs).unwrap();

    println!(
        "{} compiles, {} unique modules, {} bytes of SPIR-V",
        permutations.len(),
        modules.len(),
        total
    );
}

/// Applies `FIXES`: rewrites the format operand of the variable's image type, which must
/// be used by that variable only.
fn fix_formats(spirv: &mut [u8]) {
    let mut words: Vec<u32> = spirv
        .chunks_exact(4)
        .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect();
    for (variable, from, to) in FIXES {
        let mut name_id = None;
        let mut pointers = BTreeMap::new();
        let mut images = BTreeMap::new();
        let mut uses: BTreeMap<u32, u32> = BTreeMap::new();
        let mut var_pointer = None;
        let mut i = 5;
        while i < words.len() {
            let count = ((words[i] >> 16) as usize).max(1);
            let ops = &words[i + 1..i + count];
            match words[i] & 0xffff {
                OP_NAME => {
                    let bytes: Vec<u8> = ops[1..].iter().flat_map(|w| w.to_le_bytes()).collect();
                    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
                    if &bytes[..end] == variable.as_bytes() {
                        name_id = Some(ops[0]);
                    }
                }
                OP_TYPE_IMAGE => {
                    images.insert(ops[0], i + 8);
                }
                OP_TYPE_POINTER => {
                    pointers.insert(ops[0], ops[2]);
                }
                OP_VARIABLE => {
                    if let Some(&image) = pointers.get(&ops[0]) {
                        *uses.entry(image).or_default() += 1;
                    }
                    if Some(ops[1]) == name_id {
                        var_pointer = Some(ops[0]);
                    }
                }
                _ => {}
            }
            i += count;
        }
        let Some(image) = var_pointer.and_then(|p| pointers.get(&p).copied()) else {
            continue;
        };
        let Some(&format_word) = images.get(&image) else {
            continue;
        };
        assert_eq!(uses.get(&image), Some(&1), "{variable}: image type shared");
        assert_eq!(
            words[format_word], from,
            "{variable}: unexpected declared format"
        );
        words[format_word] = to;
    }
    for (chunk, word) in spirv.chunks_exact_mut(4).zip(words) {
        chunk.copy_from_slice(&word.to_le_bytes());
    }
}

fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    for &b in bytes {
        h = (h ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

// SPIR-V opcodes, decorations and storage classes read by `reflect`.
const OP_NAME: u32 = 5;
const OP_TYPE_IMAGE: u32 = 25;
const OP_TYPE_SAMPLER: u32 = 26;
const OP_TYPE_SAMPLED_IMAGE: u32 = 27;
const OP_TYPE_ARRAY: u32 = 28;
const OP_TYPE_RUNTIME_ARRAY: u32 = 29;
const OP_TYPE_STRUCT: u32 = 30;
const OP_TYPE_POINTER: u32 = 32;
const OP_CONSTANT: u32 = 43;
const OP_VARIABLE: u32 = 59;
const OP_DECORATE: u32 = 71;
const OP_MEMBER_DECORATE: u32 = 72;
const DECORATION_BLOCK: u32 = 2;
const DECORATION_BUFFER_BLOCK: u32 = 3;
const DECORATION_NON_WRITABLE: u32 = 24;
const DECORATION_BINDING: u32 = 33;
const DECORATION_DESCRIPTOR_SET: u32 = 34;
const STORAGE_UNIFORM_CONSTANT: u32 = 0;
const STORAGE_UNIFORM: u32 = 2;
const STORAGE_STORAGE_BUFFER: u32 = 12;
const DIM_BUFFER: u32 = 5;

#[derive(Clone, Copy)]
enum Ty {
    Image {
        dim: u32,
        sampled: u32,
    },
    Sampler,
    SampledImage,
    Array(u32, u32),
    RuntimeArray(u32),
    /// Member count.
    Struct(u32),
    Pointer(u32),
}

/// The descriptor bindings of a module, classified as SPIRV-Reflect and the SDK's compiler
/// do (`glsl_compiler.cpp`, `ExtractReflectionData`).
fn reflect(spirv: &[u8]) -> Reflection {
    let words: Vec<u32> = spirv
        .chunks_exact(4)
        .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect();
    assert_eq!(words.first(), Some(&0x0723_0203), "not SPIR-V");
    let mut names: BTreeMap<u32, String> = BTreeMap::new();
    let mut types: BTreeMap<u32, Ty> = BTreeMap::new();
    let mut constants: BTreeMap<u32, u32> = BTreeMap::new();
    let mut bindings: BTreeMap<u32, u32> = BTreeMap::new();
    let mut sets: BTreeMap<u32, u32> = BTreeMap::new();
    let mut blocks: BTreeMap<u32, u32> = BTreeMap::new();
    let mut non_writable: BTreeMap<u32, bool> = BTreeMap::new();
    let mut members_read_only: BTreeMap<u32, u32> = BTreeMap::new();
    let mut variables = Vec::new();
    let mut i = 5;
    while i < words.len() {
        let count = (words[i] >> 16) as usize;
        let op = words[i] & 0xffff;
        let ops = &words[i + 1..i + count.max(1)];
        match op {
            OP_NAME => {
                let bytes: Vec<u8> = ops[1..].iter().flat_map(|w| w.to_le_bytes()).collect();
                let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
                names.insert(ops[0], String::from_utf8_lossy(&bytes[..end]).into_owned());
            }
            OP_TYPE_IMAGE => {
                types.insert(
                    ops[0],
                    Ty::Image {
                        dim: ops[2],
                        sampled: ops[6],
                    },
                );
            }
            OP_TYPE_SAMPLER => {
                types.insert(ops[0], Ty::Sampler);
            }
            OP_TYPE_SAMPLED_IMAGE => {
                types.insert(ops[0], Ty::SampledImage);
            }
            OP_TYPE_ARRAY => {
                types.insert(ops[0], Ty::Array(ops[1], ops[2]));
            }
            OP_TYPE_RUNTIME_ARRAY => {
                types.insert(ops[0], Ty::RuntimeArray(ops[1]));
            }
            OP_TYPE_STRUCT => {
                types.insert(ops[0], Ty::Struct(ops.len() as u32 - 1));
            }
            OP_TYPE_POINTER => {
                types.insert(ops[0], Ty::Pointer(ops[2]));
            }
            OP_CONSTANT => {
                constants.insert(ops[1], ops[2]);
            }
            OP_VARIABLE => variables.push((ops[0], ops[1], ops[2])),
            OP_DECORATE => match ops[1] {
                DECORATION_BINDING => {
                    bindings.insert(ops[0], ops[2]);
                }
                DECORATION_DESCRIPTOR_SET => {
                    sets.insert(ops[0], ops[2]);
                }
                DECORATION_BLOCK | DECORATION_BUFFER_BLOCK => {
                    blocks.insert(ops[0], ops[1]);
                }
                DECORATION_NON_WRITABLE => {
                    non_writable.insert(ops[0], true);
                }
                _ => {}
            },
            OP_MEMBER_DECORATE if ops[2] == DECORATION_NON_WRITABLE => {
                *members_read_only.entry(ops[0]).or_insert(0) += 1;
            }
            _ => {}
        }
        i += count.max(1);
    }

    let mut out = Reflection::default();
    let mut all: Vec<(u32, u32, u8, Resource)> = Vec::new();
    for (pointer, id, storage) in variables {
        if ![
            STORAGE_UNIFORM_CONSTANT,
            STORAGE_UNIFORM,
            STORAGE_STORAGE_BUFFER,
        ]
        .contains(&storage)
        {
            continue;
        }
        let Some(&binding) = bindings.get(&id) else {
            continue;
        };
        let set = sets.get(&id).copied().unwrap_or(0);
        let Some(Ty::Pointer(mut ty)) = types.get(&pointer).copied() else {
            continue;
        };
        let mut count = 1;
        loop {
            match types.get(&ty).copied() {
                Some(Ty::Array(element, length)) => {
                    count *= constants.get(&length).copied().unwrap_or(1);
                    ty = element;
                }
                Some(Ty::RuntimeArray(element)) => {
                    count = 0;
                    ty = element;
                }
                _ => break,
            }
        }
        // 0 CBV, 1 SRV texture, 2 UAV texture, 3 SRV buffer, 4 UAV buffer, 5 sampler.
        let class = match types.get(&ty).copied() {
            Some(Ty::Struct(members)) => match (storage, blocks.get(&ty).copied()) {
                (STORAGE_UNIFORM, Some(DECORATION_BLOCK)) => 0,
                (STORAGE_UNIFORM, Some(DECORATION_BUFFER_BLOCK)) | (STORAGE_STORAGE_BUFFER, _) => {
                    // Read-only (NonWritable on the variable or on every member): an SRV.
                    if non_writable.get(&id).copied().unwrap_or(false)
                        || members_read_only.get(&ty).copied().unwrap_or(0) == members
                    {
                        3
                    } else {
                        4
                    }
                }
                _ => panic!("unsupported block"),
            },
            Some(Ty::Image { dim, sampled }) => {
                assert_ne!(dim, DIM_BUFFER, "texel buffers are not supported");
                if sampled == 2 { 2 } else { 1 }
            }
            Some(Ty::Sampler) => 5,
            Some(Ty::SampledImage) => panic!("combined image samplers are not supported"),
            _ => panic!("unsupported resource type"),
        };
        let name = names
            .get(&id)
            .filter(|n| !n.is_empty())
            .or_else(|| names.get(&ty))
            .cloned()
            .unwrap_or_default();
        all.push((
            set,
            binding,
            class,
            Resource {
                name,
                binding,
                count,
                set,
            },
        ));
    }
    all.sort_by_key(|(set, binding, _, _)| (*set, *binding));
    for (_, _, class, resource) in all {
        match class {
            0 => out.cbv.push(resource),
            1 => out.srv_texture.push(resource),
            2 => out.uav_texture.push(resource),
            3 => out.srv_buffer.push(resource),
            4 => out.uav_buffer.push(resource),
            _ => out.sampler.push(resource),
        }
    }
    out
}
