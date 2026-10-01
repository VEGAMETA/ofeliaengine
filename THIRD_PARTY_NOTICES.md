# Third-party notices

Every third-party dependency, vendored source file, or adapted piece of code is listed
here with its license (CLAUDE.md §15). Update this file in the same commit that adds
the dependency.

## Rust crates (runtime dependencies)

| Crate | License | Used for |
|---|---|---|
| windows-sys 0.61 (+ windows-link 0.2) | MIT OR Apache-2.0 | Win32 bindings in `engine_platform` (ADR 0004); loading and signature checks of the Streamline DLLs in `engine_dlss` (ADR 0063) |
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
| meshopt 0.6.2 (+ bitflags 2, float-cmp 0.10, num-traits 0.2, thiserror 2 + thiserror-impl) | MIT OR Apache-2.0 (float-cmp MIT) | mesh simplification for detail levels in `engine_assets` (ADR 0053) |
| zstd 0.13.3 (+ zstd-safe 7.3, zstd-sys 2.1) | MIT (zstd-safe, zstd-sys MIT OR Apache-2.0) | compression of pack entries in `engine_assets` (ADR 0056) |
| zstd 1.5.7 (© Meta Platforms, Inc. and affiliates; vendored inside zstd-sys, built by cc) | BSD-3-Clause | the compression library behind the `zstd` crates |
| meshoptimizer 0.25 (© Arseny Kapoulkine; vendored inside meshopt, built by cc) | MIT | the simplifier and vertex cache optimizer behind meshopt |
| lewton 0.10.2 (+ tinyvec 1.13, byteorder 1.5) | MIT OR Apache-2.0 (tinyvec Zlib OR Apache-2.0 OR MIT; byteorder Unlicense OR MIT) | Ogg Vorbis decoding in `engine_audio_decode` (ADR 0051) |
| ogg 0.8.0 | BSD-3-Clause | Ogg pages of Opus files in `engine_audio_decode` (ADR 0051) |
| claxon 0.4.3 | Apache-2.0 | FLAC decoding in `engine_audio_decode` (ADR 0051) |
| log 0.4.34 | MIT OR Apache-2.0 | the logging facade `engine_log` implements (ADR 0061) |
| crash-handler 0.8.1 + crash-context 0.8 (© Embark Studios; + parking_lot 0.12, lock_api, parking_lot_core, scopeguard, smallvec, libc, cfg-if) | MIT OR Apache-2.0 (crash-context MIT) | hooks the process's crash paths in `engine_crash` (ADR 0061) |
| minidumper 0.11.0 + minidump-writer 0.13.0 + minidump-common 0.26 (© Embark Studios; + polling 3, concurrent-queue, crossbeam-utils, pin-project-lite, scroll 0.12 + scroll_derive, goblin 0.9, plain, error-graph, failspot, debugid, uuid, range-map, num-traits, bitflags, serde, serde_json, itoa, memchr, zmij, thiserror 2, windows-sys 0.61) | MIT OR Apache-2.0 (minidump-writer, minidump-common, scroll, goblin, error-graph, failspot, crash-context, zmij MIT; debugid Apache-2.0; memchr Unlicense OR MIT) | the crash monitor process and its minidumps in `engine_crash` (ADR 0061) |

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
| libopus (© 2001-2023 Xiph.Org, Skype Limited, Octasic, Jean-Marc Valin, Timothy B. Terriberry, CSIRO, Gregory Maxwell, Mark Borgerding, Erik de Castro Lopo, Mozilla, Amazon) | 1.5.2 | BSD-3-Clause (`third_party/opus/COPYING`) | Opus decoding in `engine_audio_decode` (ADR 0051) |
| AMD FidelityFX SDK (© 2024 Advanced Micro Devices, Inc.; only the FSR 3.1 upscaler, the Vulkan backend and their shared code and GLSL) | v1.1.4, commit c6efa6bf7f20 (FSR 3.1.4) | MIT (`third_party/FidelityFX-SDK/LICENSE.txt`) | FSR 3.1 upscaling in `engine_fsr` (ADR 0060); the SPIR-V in `crates/engine_fsr/shaders` is compiled from its GLSL |
| Vulkan-Headers (© 2015-2023 The Khronos Group Inc.; `vulkan.h`, `vulkan_core.h`, `vk_platform.h`, `vk_video/`) | v1.4.350, commit 8864cdc896bb | Apache-2.0 OR MIT (`third_party/Vulkan-Headers/LICENSE.md`) | the C headers the FidelityFX SDK's Vulkan backend compiles against in `engine_fsr` |

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
| cc 1.5 (+ find-msvc-tools, jobserver, libc, shlex) | MIT OR Apache-2.0 | compiles the vendored Jolt, JoltC, Box2D and FidelityFX SDK sources, and Luau (through luau0-src) |

## Rust crates (compile-time only: procedural macros)

| Crate | License | Used for |
|---|---|---|
| bytemuck_derive 1.12 (+ proc-macro2, quote, syn 3, unicode-ident) | Zlib OR Apache-2.0 OR MIT (the others MIT OR Apache-2.0; unicode-ident also Unicode-3.0) | `#[derive(Pod, Zeroable)]` with compile-time padding checks (ADR 0017) |
| num-derive 0.4, smart-default 0.7, serde_derive, thiserror-impl 2 (+ syn 2) | MIT OR Apache-2.0 (smart-default MIT) | derives inside minidump-writer (ADR 0061) |

## Rust crates (dev-dependencies only, not shipped)

| Crate | License | Used for |
|---|---|---|
| rayon 1.12 (+ rayon-core, crossbeam-deque, crossbeam-epoch, crossbeam-utils, either) | MIT OR Apache-2.0 | baseline in `engine_core` job-system benchmarks (ADR 0002) |
| loom 0.7 (only with `--cfg loom`) | MIT | model checking of the lock-free queues in `engine_core::jobs` and the log ring in `engine_log` |
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
| NVIDIA Streamline 2.x (`sl.interposer.dll`, `sl.common.dll`, `sl.dlss.dll`) and NVIDIA DLSS (`nvngx_dlss.dll`), supplied by the user or the game in `plugins/nvidia/` (never committed or shipped by the engine) | Streamline's code MIT; NVIDIA's signed binaries under the NVIDIA RTX SDKs License (distribution, notification and attribution terms bind whoever ships them, `docs/dev-guide/upscalers.md`) | DLSS Super Resolution in `engine_dlss` (ADR 0063) |

## Adapted code

Constants and formulas taken into our own code (no files copied).

| What | Where | Source | License |
|---|---|---|---|
| ACES 1.0 RRT + sRGB ODT fit (matrices and rational curve) by Stephen Hill | `engine_render/src/post.rs` (`ACES_IN`, `ACES_OUT`, `aces`), `engine_render/shaders/tonemap.slang` | MJP and David Neubelt, Baking Lab, `BakingLab/ACES.hlsl` | MIT (Copyright (c) 2016 MJP) |
| The ACES transforms that fit approximates | same | Academy Color Encoding System, A.M.P.A.S. | ACES license (below) |
| AgX inset/outset matrices for Rec.2020 and the polynomial sigmoid's coefficients | `engine_render/src/post.rs` (`AGX_IN`, `AGX_OUT`, `agx_sigmoid`), `engine_render/shaders/tonemap.slang` | Filament `filament/src/ToneMapper.cpp` (matrices from Blender's AgX via EaryChow/AgX_LUT_Gen, no license, accepted by the team, ADR 0048; sigmoid adapted from B. Wrensch's minimal AgX) | Apache-2.0 (Filament) |
| The Streamline ABI: structure layouts, GUIDs, enumeration values and function signatures of `sl.h`, `sl_consts.h`, `sl_core_api.h`, `sl_core_types.h`, `sl_dlss.h`, `sl_helpers_vk.h`, `sl_result.h`, `sl_struct.h`, `sl_version.h` (v2.14.1), declared in Rust | `engine_dlss/src/sl.rs` | NVIDIA Streamline, github.com/NVIDIAGameWorks/Streamline | MIT (Copyright (c) 2022-2023 NVIDIA CORPORATION, below) |
| GTAO as structured by XeGTAO: the depth MIP filter, the main pass's slice and horizon math, the constants (radius × 1.457, falloff 0.615, MIP offset 3.3, the 0.03 floor, ÷ 1.5 before the blur), R2 + Hilbert noise, the edges and their 2-bit packing, the denoise weights | `engine_render/src/ao.rs`, `engine_render/shaders/ao.slang` | Intel XeGTAO, `XeGTAO.h`, `XeGTAO.hlsli` (after Jimenez et al., SIGGRAPH 2016) | MIT (Copyright (C) 2016-2021, Intel Corporation) |
| LDR-FLIP: its constants, separable spatial and feature filters, colour transforms and error remapping, ported to Rust | `tests/golden/src/flip.rs` (test-only crate `engine_golden`) | NVIDIA `NVlabs/flip`, `src/cpp/FLIP.h` (Andersson et al., HPG 2020) | BSD-3-Clause (Copyright (c) 2020-2025, NVIDIA CORPORATION & AFFILIATES) |

### MIT license (Baking Lab)

```
Copyright (c) 2016 MJP

Permission is hereby granted, free of charge, to any person obtaining a copy of this
software and associated documentation files (the "Software"), to deal in the Software
without restriction, including without limitation the rights to use, copy, modify,
merge, publish, distribute, sublicense, and/or sell copies of the Software, and to permit
persons to whom the Software is furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all copies or
substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR IMPLIED,
INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY, FITNESS FOR A PARTICULAR
PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE AUTHORS OR COPYRIGHT HOLDERS BE LIABLE
FOR ANY CLAIM, DAMAGES OR OTHER LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR
OTHERWISE, ARISING FROM, OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER
DEALINGS IN THE SOFTWARE.
```

### MIT license (NVIDIA Streamline)

```
Copyright (c) 2022-2023 NVIDIA CORPORATION. All rights reserved

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
```

### MIT license (XeGTAO)

```
Copyright (C) 2016-2021, Intel Corporation

Permission is hereby granted, free of charge, to any person obtaining a copy of this
software and associated documentation files (the "Software"), to deal in the Software
without restriction, including without limitation the rights to use, copy, modify,
merge, publish, distribute, sublicense, and/or sell copies of the Software, and to permit
persons to whom the Software is furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all copies or
substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR IMPLIED,
INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY, FITNESS FOR A PARTICULAR
PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE AUTHORS OR COPYRIGHT HOLDERS BE LIABLE
FOR ANY CLAIM, DAMAGES OR OTHER LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR
OTHERWISE, ARISING FROM, OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER
DEALINGS IN THE SOFTWARE.
```

### BSD 3-Clause license (FLIP)

```
Copyright (c) 2020-2025, NVIDIA CORPORATION & AFFILIATES. All rights reserved.

Redistribution and use in source and binary forms, with or without
modification, are permitted provided that the following conditions are met:

1. Redistributions of source code must retain the above copyright notice, this
list of conditions and the following disclaimer.

2. Redistributions in binary form must reproduce the above copyright notice,
this list of conditions and the following disclaimer in the documentation
and/or other materials provided with the distribution.

3. Neither the name of the copyright holder nor the names of its
contributors may be used to endorse or promote products derived from
this software without specific prior written permission.

THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS "AS IS"
AND ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO, THE
IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE ARE
DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT HOLDER OR CONTRIBUTORS BE LIABLE
FOR ANY DIRECT, INDIRECT, INCIDENTAL, SPECIAL, EXEMPLARY, OR CONSEQUENTIAL
DAMAGES (INCLUDING, BUT NOT LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR
SERVICES; LOSS OF USE, DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER
CAUSED AND ON ANY THEORY OF LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY,
OR TORT (INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE
OF THIS SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.
```

### ACES license (A.M.P.A.S.)

```
Academy Color Encoding System (ACES) software and tools are provided by the Academy under
the following terms and conditions: A worldwide, royalty-free, non-exclusive right to
copy, modify, create derivatives, and use, in source and binary forms, is hereby granted,
subject to acceptance of this license.

Copyright © 2015 Academy of Motion Picture Arts and Sciences (A.M.P.A.S.). Portions
contributed by others as indicated. All rights reserved.

Performance of any of the aforementioned acts indicates acceptance to be bound by the
following terms and conditions:

* Copies of source code, in whole or in part, must retain the above copyright notice,
  this list of conditions and the Disclaimer of Warranty.
* Use in binary form must retain the above copyright notice, this list of conditions and
  the Disclaimer of Warranty in the documentation and/or other materials provided with
  the distribution.
* Nothing in this license shall be deemed to grant any rights to trademarks, copyrights,
  patents, trade secrets or any other intellectual property of A.M.P.A.S. or any
  contributors, except as expressly stated herein.
* Neither the name "A.M.P.A.S." nor the name of any other contributors to this software
  may be used to endorse or promote products derivative of or based on this software
  without express prior written permission of A.M.P.A.S. or the contributors, as
  appropriate.

This license shall be construed pursuant to the laws of the State of California, and any
disputes related thereto shall be subject to the jurisdiction of the courts therein.

Disclaimer of Warranty.

* Use in binary form must retain the above copyright notice, this list of
conditions and the Disclaimer of Warranty in the documentation and/or other
materials provided with the distribution.

* Nothing in this license shall be deemed to grant any rights to trademarks,
copyrights, patents, trade secrets or any other intellectual property of
A.M.P.A.S. or any contributors, except as expressly stated herein.

* Neither the name "A.M.P.A.S." nor the name of any other contributors to this
software may be used to endorse or promote products derivative of or based on
this software without express prior written permission of A.M.P.A.S. or the
contributors, as appropriate.

This license shall be construed pursuant to the laws of the State of
California, and any disputes related thereto shall be subject to the
jurisdiction of the courts therein.

Disclaimer of Warranty: THIS SOFTWARE IS PROVIDED BY A.M.P.A.S. AND CONTRIBUTORS
"AS IS" AND ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO,
THE IMPLIED WARRANTIES OF MERCHANTABILITY, FITNESS FOR A PARTICULAR PURPOSE, AND
NON-INFRINGEMENT ARE DISCLAIMED. IN NO EVENT SHALL A.M.P.A.S., OR ANY
CONTRIBUTORS OR DISTRIBUTORS, BE LIABLE FOR ANY DIRECT, INDIRECT, INCIDENTAL,
SPECIAL, EXEMPLARY, RESITUTIONARY, OR CONSEQUENTIAL DAMAGES (INCLUDING, BUT NOT
LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR SERVICES; LOSS OF USE, DATA, OR
PROFITS; OR BUSINESS INTERRUPTION) HOWEVER CAUSED AND ON ANY THEORY OF
LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY, OR TORT (INCLUDING NEGLIGENCE
OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE OF THIS SOFTWARE, EVEN IF
ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.

WITHOUT LIMITING THE GENERALITY OF THE FOREGOING, THE ACADEMY SPECIFICALLY
DISCLAIMS ANY REPRESENTATIONS OR WARRANTIES WHATSOEVER RELATED TO PATENT OR
OTHER INTELLECTUAL PROPERTY RIGHTS IN THE ACADEMY COLOR ENCODING SYSTEM, OR
APPLICATIONS THEREOF, HELD BY PARTIES OTHER THAN A.M.P.A.S.,WHETHER DISCLOSED OR
UNDISCLOSED.
```
