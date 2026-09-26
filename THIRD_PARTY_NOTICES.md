# Third-party notices

Every third-party dependency, vendored source file, or adapted piece of code is listed
here with its license (CLAUDE.md §15). Update this file in the same commit that adds
the dependency.

## Rust crates (runtime dependencies)

| Crate | License | Used for |
|---|---|---|
| windows-sys 0.61 (+ windows-link 0.2) | MIT OR Apache-2.0 | Win32 bindings in `engine_platform` (ADR 0004) |
| ash 0.38 | MIT OR Apache-2.0 | Vulkan bindings in `engine_rhi` (ADR 0006) |
| libloading 0.8 (+ cfg-if 1) | ISC (cfg-if: MIT OR Apache-2.0) | loads the Vulkan loader at run time (dependency of ash) |
| glam 0.33 | MIT OR Apache-2.0 | vectors, matrices, quaternions in `engine_core::math` (ADR 0012) |

Only compiled into builds with an `engine_profiler/tracy*` feature (`cargo profiling`):

| Crate | License | Used for |
|---|---|---|
| tracy-client 0.19 | MIT OR Apache-2.0 | Rust API of the Tracy client (ADR 0003) |
| tracy-client-sys 0.30 | (MIT OR Apache-2.0) AND BSD-3-Clause | builds the bundled Tracy 0.14.1 client (C++, BSD-3-Clause, © Bartosz Taudul) |
| windows-targets 0.53 and `windows_*` 0.53 | MIT OR Apache-2.0 | Windows import libraries used by tracy-client-sys |

## Rust crates (dev-dependencies only, not shipped)

| Crate | License | Used for |
|---|---|---|
| rayon 1.12 (+ rayon-core, crossbeam-deque, crossbeam-epoch, crossbeam-utils, either) | MIT OR Apache-2.0 | baseline in `engine_core` job-system benchmarks (ADR 0002) |
| loom 0.7 (only with `--cfg loom`) | MIT | model checking of the lock-free queues in `engine_core::jobs` |
| gpu-allocator 0.28 (+ log, presser, thiserror, thiserror-impl, proc-macro2, quote, syn, unicode-ident) | MIT OR Apache-2.0 (unicode-ident also Unicode-3.0) | baseline in the `engine_rhi` memory allocator benchmark (ADR 0010) |

## Loaded at run time, not distributed

| Component | License | Used for |
|---|---|---|
| Slang compiler library (from the user's Vulkan SDK or `ENGINE_SLANG_LIBRARY`) | Apache-2.0 WITH LLVM-exception | shader compilation in the editor and tools (ADR 0008) |

## Vendored sources (`/third_party`)

_None yet._

## Adapted code

_None yet._
