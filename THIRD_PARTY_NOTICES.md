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
| bytemuck 1.25 | Zlib OR Apache-2.0 OR MIT | plain-data proofs for ECS components that are snapshotted and hashed byte-wise (ADR 0017) |
| harfrust 0.13.3 (+ bitflags 2, once_cell 1, smallvec 1) | MIT (the others MIT OR Apache-2.0) | text shaping in `engine_text` (ADR 0025) |
| skrifa 0.46 (+ read-fonts 0.43, font-types 0.12) | MIT OR Apache-2.0 | font tables, metrics, charmap and glyph outlines in `engine_text` |
| unicode-linebreak 0.1.5 | MIT OR Apache-2.0 | UAX #14 line break opportunities in `engine_text` |
| unicode-script 0.5.8 | MIT OR Apache-2.0 | script itemization in `engine_text` |

Only compiled into builds with an `engine_profiler/tracy*` feature (`cargo profiling`):

| Crate | License | Used for |
|---|---|---|
| tracy-client 0.19 | MIT OR Apache-2.0 | Rust API of the Tracy client (ADR 0003) |
| tracy-client-sys 0.30 | (MIT OR Apache-2.0) AND BSD-3-Clause | builds the bundled Tracy 0.14.1 client (C++, BSD-3-Clause, © Bartosz Taudul) |
| windows-targets 0.53 and `windows_*` 0.53 | MIT OR Apache-2.0 | Windows import libraries used by tracy-client-sys |

## Vendored sources (`third_party/`)

| Project | Version | License | Used for |
|---|---|---|---|
| Jolt Physics (© 2021 Jorrit Rouwe) | v5.6.0 | MIT | reference physics backend `engine_physics_jolt` (ADR 0018) |
| JoltC (© Amer Koleci and contributors) | commit 886e088675ba | MIT | C API over Jolt, bound by `engine_physics_jolt` |
| Go fonts (© 2016 Bigelow & Holmes Inc., for the Go project) | golang/image commit b06f1de3f490 | BSD-3-Clause | Go Regular, Bold and Mono: the engine's bundled fonts and the text test font (`third_party/fonts/go`, embedded by `engine_text::fonts`) |

## Rust crates (build dependencies)

| Crate | License | Used for |
|---|---|---|
| cc 1.5 (+ find-msvc-tools, jobserver, libc, shlex) | MIT OR Apache-2.0 | compiles the vendored Jolt and JoltC sources |

## Rust crates (compile-time only: procedural macros)

| Crate | License | Used for |
|---|---|---|
| bytemuck_derive 1.12 (+ proc-macro2, quote, syn 3, unicode-ident) | Zlib OR Apache-2.0 OR MIT (the others MIT OR Apache-2.0; unicode-ident also Unicode-3.0) | `#[derive(Pod, Zeroable)]` with compile-time padding checks (ADR 0017) |

## Rust crates (dev-dependencies only, not shipped)

| Crate | License | Used for |
|---|---|---|
| rayon 1.12 (+ rayon-core, crossbeam-deque, crossbeam-epoch, crossbeam-utils, either) | MIT OR Apache-2.0 | baseline in `engine_core` job-system benchmarks (ADR 0002) |
| loom 0.7 (only with `--cfg loom`) | MIT | model checking of the lock-free queues in `engine_core::jobs` |
| gpu-allocator 0.28 (+ log, presser, thiserror, thiserror-impl, proc-macro2, quote, syn, unicode-ident) | MIT OR Apache-2.0 (unicode-ident also Unicode-3.0) | baseline in the `engine_rhi` memory allocator benchmark (ADR 0010) |

## Research harnesses (outside the workspace, never linked into the engine)

`bench/ecs-compare` (ADR 0017) builds these with its own `Cargo.lock`. All pass the
workspace `deny.toml` license allow-list (checked 2026-09-26).

| Crate | License | Used for |
|---|---|---|
| hecs 0.11.1 | MIT OR Apache-2.0 | ECS comparison |
| bevy_ecs 0.18.1 (and its bevy_* dependencies) | MIT OR Apache-2.0 | ECS comparison |
| flecs_ecs 0.2.2 (+ flecs_ecs_sys, bundled flecs C sources) | MIT | ECS comparison |

`bench/text-compare` (ADR 0025) builds these with its own `Cargo.lock`:

| Crate | License | Used for |
|---|---|---|
| parley 0.11.1 (+ fontique, parlance, ICU4X crates and data) | Apache-2.0 OR MIT (ICU4X: Unicode-3.0) | text layout comparison |
| cosmic-text 0.19.0 (+ fontdb, swash, older harfrust and skrifa) | MIT OR Apache-2.0 | text layout comparison |

## Loaded at run time, not distributed

| Component | License | Used for |
|---|---|---|
| Slang compiler library (from the user's Vulkan SDK or `ENGINE_SLANG_LIBRARY`) | Apache-2.0 WITH LLVM-exception | shader compilation in the editor and tools (ADR 0008) |

## Adapted code

_None yet._
