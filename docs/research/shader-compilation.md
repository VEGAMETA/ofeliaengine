# Shader compilation with Slang: research

Date: 2026-09-25. Input for [ADR 0008](../adr/0008-shader-compilation.md). No code was copied.

## Sources

| Source | Version checked | License | What we looked at |
|---|---|---|---|
| [Slang](https://github.com/shader-slang/slang) `include/slang.h`, `include/slang-deprecated.h` | commit `6eb8978` (2026-09-24) | Apache-2.0 WITH LLVM-exception | C API, COM API, enums |
| `libslang-compiler` 2026.12 (from the slangpy 0.43.1 wheel) | 2026.12 | same | measurements in the container |
| Vulkan SDK 1.4.350 (ref-1) | 1.4.350.0 | — | ships `slangc` and the Slang library |

## API surface

- **COM API** (`slang_createGlobalSession`, `IGlobalSession`, `ISession`, `IModule`,
  `IComponentType`). This is the current, recommended API. Using it from Rust needs vtables
  declared by hand, in exactly the order `slang.h` declares the methods (the `shader-slang`
  crate 0.1 does this and links Slang at build time).
- **Flat C API** (`spCreateSession`, `spCreateCompileRequest`, `spAddTranslationUnit`,
  `spAddEntryPoint`, `spCompile`, `spGetEntryPointCode`, `spGetDependencyFilePath`, ...). It was
  moved to `slang-deprecated.h` ("maintained for backward compatibility") but is still exported
  in 2026.12. It is `extern "C"` and trivially bound with `libloading`.
- `spGetBuildTagString` (in `slang.h`, not deprecated) returns the version, for example `2026.12`.
- `spGetDependencyFileCount` / `spGetDependencyFilePath` list every file a compilation read,
  including imports. This is what makes exact cache keys and hot-reload dependency tracking
  possible.
- **Pitfall:** driven only through `spProcessCommandLineArguments` (slangc-style arguments),
  entry-point code is produced only when an output file (`-o`) is given, and then it is also
  written to disk. The explicit API calls (`spSetCodeGenTarget`, `spAddEntryPoint`, ...) produce
  code in memory. The only command-line flag we still pass is `-fvk-use-entrypoint-name`;
  without it the SPIR-V entry point is renamed `main`.

## Measurements (container, 4 vCPU, Slang 2026.12)

| Operation | Time |
|---|---|
| Create a session (loads the core module) | 178–262 ms |
| Compile one entry point in an existing session | 14 ms (Python ctypes loop); 19–27 ms from Rust with I/O |
| New session + compile (what every `slangc` process does, excluding process start) | 242 ms |

A `slangc` subprocess per compilation costs about 17× more than a compile in a warm session.
Hot reload (≤ 500 ms, §5) and "Compile All Shaders" across thousands of variants both need
warm sessions.

## What we take

- In-process compilation through the flat C API, loaded at run time. Games never load Slang;
  they ship SPIR-V.
- One session per compiling thread, reused.
- The explicit request API, plus the one flag.
- A two-level content-addressed cache, keyed by the request (compiler version, options, entry
  points, source content) and then by the dependencies' content hashes, which Slang reports.

## What we don't (yet)

- The COM API. Switch when Slang announces removal of the flat API, or when we need features
  only it offers (precompiled modules, link-time specialization, reflection of the linked
  program).
- Reflection. The bindless design (§6) needs little of it; add it with the material system.
