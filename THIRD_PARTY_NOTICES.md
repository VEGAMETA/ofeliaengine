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
| mlua 0.12.1 + mlua-sys 0.12 (+ bstr, either, num-traits, parking_lot, rustc-hash) | MIT (the others MIT OR Apache-2.0, bstr MIT OR Apache-2.0) | Luau VM bindings in `engine_script` (ADR 0032) |
| Luau 0.736 (© Roblox Corporation, via luau0-src 0.21, vendored C++ sources built by cc) | MIT | the scripting language and its native code generator (ADR 0032) |
| gltf 1.4.1 + gltf-json + gltf-derive (+ serde, serde_json, byteorder, lazy_static, inflections, itoa, memchr, zmij) | MIT OR Apache-2.0 (byteorder and memchr Unlicense OR MIT; inflections, zmij MIT) | glTF 2.0 parsing in `engine_assets` (ADR 0033) |
| png 0.18.1 (+ fdeflate 0.3, flate2 1.1, miniz_oxide 0.8 and 0.9, crc32fast 1.5, simd-adler32 0.3, adler2 2) | MIT OR Apache-2.0 (miniz_oxide MIT OR Zlib OR Apache-2.0; simd-adler32 MIT; adler2 0BSD OR MIT OR Apache-2.0) | PNG decoding of textures in `engine_assets` (ADR 0040) |
| zune-jpeg 0.5.15 (+ zune-core 0.5) | MIT OR Apache-2.0 OR Zlib | JPEG decoding of textures in `engine_assets` (ADR 0040) |
| block_compression 0.10.0 (© Nils Hasenbanck; a port of Intel's ISPC Texture Compressor, © Intel Corporation) | MIT | BC7 and BC5 texture compression in `engine_assets` (ADR 0041) |
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
| Box2D (© 2022 Erin Catto) | v3.1.1 | MIT | 2D physics, `engine_physics2d` (ADR 0028) |

## Fonts and icons

Every font and icon set the engine ships, unmodified, under `third_party/fonts/`, embedded
by `engine_text::fonts`. OFL-1.1 and CC BY 4.0 are allowed for fonts and icons only, on the
condition of this section (ADR 0036). License texts are next to each font.

| Font or icon set | Author | Version | License | Upstream | Used for |
|---|---|---|---|---|---|
| Noto Sans | © 2022 The Noto Project Authors | 2.015 | OFL-1.1 | https://github.com/notofonts/latin-greek-cyrillic | editor UI text (`NOTO_SANS`) |
| JetBrains Mono | © 2020 The JetBrains Mono Project Authors | 2.305 | OFL-1.1 | https://github.com/JetBrains/JetBrainsMono | code, console; arrows and symbols in the UI stack (`JETBRAINS_MONO`) |
| Go fonts (Regular, Bold, Mono) | © 2016 Bigelow & Holmes Inc., for the Go project | golang/image commit b06f1de3f490 | BSD-3-Clause | https://go.googlesource.com/image | test font of `engine_text` (`GO_*`) |
| Symbols Nerd Font (the font and scripts) | © 2014 Ryan L McIntyre | Nerd Fonts 3.5.1 | MIT | https://github.com/ryanoasis/nerd-fonts | icons (`NERD_SYMBOLS`), with the sets below |
| ↳ Codicons | © Microsoft Corporation | 0.0.45 | CC BY 4.0 | https://github.com/microsoft/vscode-codicons | icons |
| ↳ Devicons | © the Devicon contributors | 2.17.0 | MIT | https://github.com/devicons/devicon | icons |
| ↳ extraglyphs (from Hack) | © Source Foundry authors | – | MIT | https://github.com/source-foundry/Hack | icons |
| ↳ Font Awesome | © Fonticons, Inc. | 6.5.1 | CC BY 4.0 (icons) | https://github.com/FortAwesome/Font-Awesome | icons |
| ↳ Font Awesome Extension | © Andre Gava | 0.0.3 | MIT | https://github.com/AndreLZGava/font-awesome-extension | icons |
| ↳ Font Logos | © Lukas W | 1.3.0 | Unlicense | https://github.com/lukas-w/font-logos | icons |
| ↳ Material Design Icons | © Pictogrammers | 2022-10-06 | Apache-2.0 | https://github.com/Templarian/MaterialDesign-Font | icons (play, pause, stop, folders) |
| ↳ Octicons | © GitHub Inc. | 18.3.0 | MIT | https://github.com/primer/octicons | icons |
| ↳ Seti-UI | © Jesse Weed | 0.8.1 | MIT | https://github.com/jesseweed/seti-ui | icons |
| ↳ Pomicons | © Gabriele Lana | 1.001 | OFL-1.1 | https://github.com/gabrielelana/pomicons | icons |
| ↳ Powerline Extra Symbols | © Ryan L McIntyre | 1.200 | MIT | https://github.com/ryanoasis/powerline-extra-symbols | icons |
| ↳ Powerline Symbols | © Kim Silkebækken and powerline contributors | 1.000 | MIT | https://github.com/powerline/powerline | icons |
| ↳ IEC Power Symbols | © Joe Loughry | 2015-02 | MIT | https://github.com/jloughry/Unicode | icons |
| ↳ Weather Icons | © Erik Flowers | 2.0.10 | OFL-1.1 | https://github.com/erikflowers/weather-icons | icons |

## Rust crates (build dependencies)

| Crate | License | Used for |
|---|---|---|
| cc 1.5 (+ find-msvc-tools, jobserver, libc, shlex) | MIT OR Apache-2.0 | compiles the vendored Jolt, JoltC and Box2D sources, and Luau (through luau0-src) |

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

`bench/texture-compress` (ADR 0041) builds these with its own `Cargo.lock`: block_compression
0.10.0 (MIT, above) and the engine's own crates.

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
