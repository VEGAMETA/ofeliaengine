# FSR 3.1 upscaling (and frame generation) on Vulkan

How AMD's FidelityFX Super Resolution 3.1 is built, what it needs, how it maps onto the
engine's inputs, and whether its frame generation fits now. Decision: ADR 0060.
Measurements: `docs/perf/history.md` and ADR 0060.

Checked on 2026-10-01.

## Sources

| Source | Version | License | Finding |
|---|---|---|---|
| [AMD FidelityFX SDK](https://github.com/GPUOpen-LibrariesAndSDKs/FidelityFX-SDK) | v1.1.4, `c6efa6b` | MIT | FSR 3.1 upscaler **3.1.4** (`FFX_FSR3UPSCALER_VERSION_*`) and frame interpolation with a DX12 and a **Vulkan** backend. The last release with Vulkan. |
| The same repository, "AMD FSR SDK" | v2.3.0, `60f4ea8` (HEAD on 2026-10-01) | MIT | FSR 4 (ML) and FSR 3.1.5+; the readme still says *"Vulkan is currently not supported in SDK"*. Same as on 2026-09-26 (`frame-generation.md`). FSR 4 stays DX12-only. |
| The SDK's documentation, `docs/techniques/super-resolution-upscaler.md` | v1.1.4 | MIT | Inputs, conventions, jitter, mip bias, quality modes. |
| The SDK's documentation, `docs/techniques/frame-interpolation-swap-chain.md` | v1.1.4 | MIT | What frame generation needs from the present path (below). |
| [Vulkan-Headers](https://github.com/KhronosGroup/Vulkan-Headers) | v1.4.350, `8864cdc` | Apache-2.0 OR MIT | The C headers the SDK's Vulkan backend compiles against (vendored). |
| glslang (`glslangValidator.exe`) | 11.12.0, the binary the SDK ships in `sdk/tools/binary_store` | BSD-3-Clause and others (open source) | Compiles the SDK's GLSL. Used once, to generate the committed SPIR-V; not linked, not shipped. |

No code from GPL projects was read.

## The SDK and how it builds

- **Layout.** `sdk/` holds the effects (`src/components/*`), the backends
  (`src/backends/{dx12,vk}`), the GPU code (`include/FidelityFX/gpu`, HLSL and GLSL) and
  tools; `ffx-api/` is a small versioned C interface over *providers* (FSR 2, FSR 3.1
  upscaling, frame generation, the frame generation swapchain), built as
  `amd_fidelityfx_vk.dll`. `PrebuiltSignedDLL/` holds AMD-signed builds of that DLL.
- **CMake.** `ffx-api/CMakeLists.txt` (`-DFFX_API_BACKEND=VK_X64`) builds the effects and
  the Vulkan backend as static libraries and links them into the DLL. It needs the Vulkan
  SDK (`find_package(Vulkan)`, 1.3.250 or newer). With the Visual Studio 2026 generator it
  configured and built here in 40 s, shaders included.
- **Shaders.** Every pass is compiled by `FidelityFX_SC.exe` into C headers of SPIR-V
  (Vulkan) or DXIL (DX12) with reflection data:
  - `FidelityFX_SC.exe` is prebuilt in `sdk/tools/binary_store`. Its source is in the SDK
    (`sdk/tools/ffx_shader_compiler`, MIT), but it links the DirectX shader compiler and
    the Agility SDK and runs a prebuilt `glslangValidator.exe` for GLSL.
  - For Vulkan it runs glslang on the GLSL passes with
    `-e CS --target-env vulkan1.2 -S comp -Os -DFFX_GLSL=1` and the permutation defines,
    deduplicates identical modules by MD5, and reflects bindings with SPIRV-Reflect.
  - Permutations: 6 option bits (Lanczos lookup table, HDR input, low-resolution motion
    vectors, jittered motion vectors, inverted depth, sharpening) × wave64 × 16-bit, per
    pass. For the FSR 3.1 upscaler that is 200 generated headers, **23 MB of text** for
    3.4 MB of unique SPIR-V.
  - On Vulkan the wave64 variants are the same modules (the backend requests the
    subgroup size), and the backend never asks for them: it reports shader model 5.1, and
    wave64 needs 6.6.
- **Runtime dependencies.** The Vulkan loader only. The backend calls six
  physical-device functions and `vkCreateBuffer` directly (it expects `vulkan-1.lib`);
  everything else goes through `vkGetDeviceProcAddr`.
- **Sizes.** The engine's build (below): 9 C++ files, a 430 KB static library, compiled in
  about 3 s on the 16 threads of ref-1 and cached.

## The API

Two levels, both in the SDK:

1. **ffx-api** (`ffx_api.h`, `ffx_upscale.h`): `ffxCreateContext` with
   `ffxCreateContextDescUpscale` chained to `ffxCreateBackendVKDesc`; `ffxDispatch` with
   `ffxDispatchDescUpscale`; `ffxQuery` for the jitter and render sizes; `ffxConfigure`.
   Structures are chained by type headers, versioned, meant for the signed DLL. The
   upscaling provider is a thin wrapper over (2). Linking it statically links every
   provider (FSR 2, frame generation, optical flow, the swapchain).
2. **The effect's own API** (`ffx_fsr3upscaler.h` + `ffx_vk.h`):
   - `ffxGetInterfaceVK` fills a backend (`FfxInterface`) over a scratch buffer;
   - `ffxFsr3UpscalerContextCreate` takes flags, the maximum render size, the output
     size, the backend and a message callback;
   - `ffxFsr3UpscalerContextDispatch` takes a `FfxFsr3UpscalerDispatchDescription`:

| Input | Contract |
|---|---|
| `color` | render size, jittered; HDR with `FFX_FSR3UPSCALER_ENABLE_HIGH_DYNAMIC_RANGE` |
| `depth` | render size, device depth; `ENABLE_DEPTH_INVERTED` for 1 at the near plane, `ENABLE_DEPTH_INFINITE` for an infinite far plane |
| `motionVectors` | render size unless `ENABLE_DISPLAY_RESOLUTION_MOTION_VECTORS`; the shaders compute `previous = uv + mv × motionVectorScale / renderSize`; `ENABLE_MOTION_VECTORS_JITTER_CANCELLATION` if they include the jitter |
| `reactive` | optional, R8: how much to distrust history (transparent, animated surfaces) |
| `transparencyAndComposition` | optional: surfaces whose shading does not follow the motion vectors (reflections, UV animation) |
| `exposure` | optional 1 × 1 texture, or `ENABLE_AUTO_EXPOSURE`; `preExposure` divides the color |
| `dilatedDepth`, `dilatedMotionVectors`, `reconstructedPrevNearestDepth` | images the caller creates from `ffxFsr3UpscalerGetSharedResourceDescriptions` (shared with frame generation); the ffx-api provider creates them itself |
| `jitterOffset` | this frame's jitter in pixels, the sequence of `ffxFsr3UpscalerGetJitterOffset` |
| `renderSize`, `upscaleSize` | per frame; render size may change below the maximum with `ENABLE_DYNAMIC_RESOLUTION` |
| `cameraNear`, `cameraFar`, `cameraFovAngleVertical`, `viewSpaceToMetersFactor` | depth linearization and motion thresholds |
| `frameTimeDelta` | milliseconds |
| `reset` | drop history (camera cut) |
| `enableSharpening`, `sharpness` | RCAS after upscaling, 0-1 |

Quality modes: Native AA 1.0×, Quality 1.5×, Balanced 1.7×, Performance 2.0×, Ultra
Performance 3.0× per axis; render size = ⌊output / ratio⌋.

Jitter: Halton(2, 3), `x = halton(i % n + 1, 2) − 0.5`, `y = halton(i % n + 1, 3) − 0.5`,
with `n = ffxFsr3UpscalerGetJitterPhaseCount(render, output)`. The documentation says
⌈8 · s²⌉; the code computes `int(8 · s²)`, which truncates (23 phases, not 24, at 1.7×). The
upscaler uses the same count internally for its locks, so the engine follows the code.

Mip bias: the documentation recommends log2(render / output) − 1 for material textures.

## Mapping onto the engine's inputs

| FSR | Engine | How |
|---|---|---|
| color | forward HDR color, RGBA16F, jittered | HDR flag |
| depth | D32, 0 at the near plane, 1 at a finite far plane (`vulkan_perspective`) | no depth flags; near, far and the vertical FOV passed per frame (`FsrCamera`) |
| motion vectors | `MotionOutput::velocity`: RG16F, render size, texture-coordinate delta current − previous (`uv − motion` is where the surface was), no jitter | `motionVectorScale = (−render width, −render height)` |
| jitter | `MotionOutput::jitter`, pixels, x right, y down: the image moved by it | passed as is |
| jitter sequence | `Temporal`: Halton(2, 3), phase `frame % n + 1` | the same sequence; `Fsr::jitter_phases` gives the SDK's count |
| reactive | the transparent pass's R8 mask (combined alpha, ADR 0054) | passed when there is one |
| transparency & composition | none yet | not passed |
| exposure | the engine's exposure is a GPU buffer for the tonemapper (ADR 0047) | `ENABLE_AUTO_EXPOSURE`, `preExposure = 1` for now |
| reset | `MotionOutput::reset`, `Fsr::reset` | passed |

Both conventions were checked by breaking them in the test: with the jitter's sign flipped
the image upscaled from half the size loses 6 dB against the supersampled reference, and
with the motion vectors' sign flipped the panning image loses 5.5 dB.

Image layouts: the render graph keeps sampled images in `READ_ONLY_OPTIMAL`
(synchronization2); the backend describes compute reads as `SHADER_READ_ONLY_OPTIMAL`. The
engine's pass transitions its inputs to what the SDK expects and back.

## Integrating the C++ library into the Rust build

1. **The upstream CMake through the `cmake` crate.** Needs CMake, a Visual Studio
   generator, the Vulkan SDK and the prebuilt `FidelityFX_SC.exe` on every machine that
   builds the engine; builds a DLL unless patched; recompiles thousands of shader
   permutations per clean build (2,560 for the upscaler alone). Rejected: slow, heavy,
   and the shader compiler is a binary.
2. **The prebuilt signed `amd_fidelityfx_vk.dll`.** No build at all, but a binary we would
   ship, against CLAUDE.md §0 ("no closed static libraries", FSR built from source as part
   of the engine, §6). Rejected.
3. **`cc` over the needed sources, SPIR-V generated once (chosen).**
   - Seven SDK files (the upscaler, the Vulkan backend, the shared helpers) and two of
     ours are compiled into one static library, cached by content hash like Jolt. The
     Vulkan headers are vendored: building the engine needs only a C++ compiler.
   - The shader blob accessor (`ffx_fsr3upscaler_shaderblobs.cpp`, which includes the
     generated headers) is replaced by ours, which serves SPIR-V from the Rust crate.
   - `examples/ffx_shaders.rs` does what `FidelityFX_SC.exe` does for the Vulkan GLSL, for
     the permutations the engine uses: HDR color, render-size motion vectors without
     jitter, standard depth; the Lanczos table and sharpening vary; 32- and 16-bit. That
     is 56 compiles and **20 unique modules, 471 KB of SPIR-V**, committed like the
     engine's own shaders. Run with the SDK's glslang 11.12.0, all 20 modules are
     byte-identical (MD5) to `FidelityFX_SC.exe`'s, and the reflection and the permutation
     → module mapping match for all 56 lookups. The Vulkan SDK's newer glslang (16.2.0)
     gives different, slightly larger modules.
   - No SDK source file is modified; `third_party/FidelityFX-SDK` keeps only the files
     used (2.7 MB).

## Problems found, and how the engine handles them

All handled outside the SDK's sources (`crates/engine_fsr/csrc`, `src/lib.rs`):

1. **Direct Vulkan calls.** The backend calls `vkGetPhysicalDevice{Properties,
   Properties2,Features,Features2,MemoryProperties}`, `vkEnumerateDeviceExtensionProperties`
   and `vkCreateBuffer` directly. The shim defines them and forwards through the engine's
   `vkGetInstanceProcAddr` and `vkGetDeviceProcAddr`: nothing links `vulkan-1.lib`.
2. **Old extension names.** The backend loads `vkGetBufferMemoryRequirements2KHR` by the
   extension's name whenever the GPU lists `VK_KHR_dedicated_allocation`. On a 1.3 device
   created without that extension the loader returns null and the backend crashes at
   context creation. The `vkGetDeviceProcAddr` handed to the backend falls back to the
   core name.
3. **Supported is not enabled.** The backend picks 16-bit shaders when the GPU supports
   `shaderFloat16`, not when the device enabled it. The engine now enables
   `shaderFloat16` and `shaderInt16` where supported (`OptionalCaps::shader_float16`), and
   the shim reports 16-bit math as unsupported when the device lacks it.
4. **Shared images.** The dispatch expects the caller to create the dilated depth, the
   dilated motion vectors and the reconstructed previous depth. Without them the backend
   clears an unregistered resource (validation errors, then a crash). The shim creates
   them as the ffx-api provider does.
5. **A storage image declared with the wrong format.** `rw_luma_history` is declared
   `rgba8` in the GLSL, but the image bound to it is RGBA16F; the validation layer warns
   that writes are undefined. The generator patches that one format operand to
   `Rgba16f` (the only change against the SDK's modules; `spirv-val` passes).
6. **Frame generation symbols.** The backend's interface references the frame
   generation swapchain; the shim stubs that one function, since the swapchain is not
   built.
7. **Per-frame CPU work.** Every dispatch creates image views for the engine's images,
   updates descriptor sets, and, with debug utils on, converts labels through
   `std::wstring_convert` (C++ heap allocations the engine's allocation test does not
   see). Recording a frame costs about 75 µs more CPU than with TAAU.
8. **No pipeline cache.** The backend creates its 11 pipelines without a
   `VkPipelineCache`: a new context (first frame, output size change) costs 11-14 ms on
   the RTX and 55-69 ms on the Radeon, driver caches permitting.

## Frame generation (FSR 3.1 frame interpolation, Vulkan)

What SDK 1.1.4 needs (`frame-interpolation-swap-chain.md`, `ffx_vk.h`):

- **Its own swapchain.** `ffxReplaceSwapchainForFrameinterpolationVK` creates a
  replacement; `ffxGetSwapchainReplacementFunctionsVK` returns the `vkCreateSwapchainKHR`,
  `vkAcquireNextImageKHR`, `vkQueuePresentKHR` etc. the application must call instead.
- **Queues** (`VkFrameInterpolationInfoFFX`): the game queue; an optional async compute
  queue; a **present queue the engine never uses** ("otherwise some deadlock can occur");
  an image acquire queue, preferably also unused. The engine's device creates one queue
  per family (graphics, compute-only, transfer-only), so there is no spare present queue.
- **Two CPU threads**, one of them spinning between the generated and the real present to
  pace them.
- **The UI apart from the scene**: a hudless color and a UI texture (or a composition
  callback run per presented frame).
- **Two more effects**: optical flow and frame interpolation, each with its own shader
  permutations (the generator would have to cover them), and the upscaler's shared images
  (which is why they exist).
- **Latency**: frame generation adds about one real frame of latency; AMD pairs it with
  Anti-Lag 2 (the engine has `VK_AMD_anti_lag`, ADR 0023), and the limiter must cap real
  frames.

Against `frame-generation.md`'s requirements, the engine still lacks a pluggable present
path (the swapchain is one type), extra queues, a hudless image with the UI in its own
target (nothing in the engine produces one yet), and FG-aware pacing. **Frame generation is deferred** (ADR 0060) until those exist; the
upscaler is independent of it.

## What we take, what we don't

- **Take:** the FSR 3.1.4 upscaler through the effect's own API, statically linked, with
  our shader generator and shim; the SDK's quality modes, jitter sequence and phase count.
- **Don't:** ffx-api and its providers (the DLL's interface, and it would link frame
  generation); the prebuilt DLL; `FidelityFX_SC.exe` and the CMake build; frame generation
  for now; FSR 4 (not on Vulkan).
- **Revisit:** a Vulkan release of the FSR SDK 2.x line (FSR 4, newer FSR 3.1), which
  would replace this integration; check every SDK release.
