# 0060. FSR 3.1 upscaling beside the engine's TAAU, built from source; frame generation later

- Status: Accepted
- Date: 2026-10-01
- Builds on: [ADR 0043](0043-motion-vectors-and-taau.md) (motion vectors, jitter, TAAU),
  [ADR 0054](0054-transparency.md) (the reactive mask),
  [ADR 0011](0011-render-graph.md) (the render graph)
- Research: [`docs/research/fsr.md`](../research/fsr.md),
  [`docs/research/frame-generation.md`](../research/frame-generation.md)

## Context

CLAUDE.md §6 plans upscalers behind the inputs the engine already produces (color,
depth, motion vectors, exposure, jitter, masks): the engine's own TAAU (ADR 0043), always
available, then **FSR 3.1 from the FidelityFX SDK (MIT), built from source**, then DLSS and
XeSS from user-supplied DLLs. §17 puts FSR 3.1 fifth among the current priorities.

The FidelityFX SDK 1.1.4 is the last release with a Vulkan backend; the FSR SDK 2.x line
(FSR 4) is still DX12-only (checked 2026-10-01). Its build expects CMake, the Vulkan SDK
and a prebuilt shader compiler, and produces a DLL (research note).

## Options

**What the engine offers:**

1. **FSR 3.1 only**, replacing TAAU. One temporal path to maintain; FSR's quality. But
   FSR costs 2.4-5.5× TAAU's GPU time (below), runs through a library we do not tune,
   and the engine loses its reference and fallback.
2. **TAAU only.** Cheapest, ours; lower quality (below), no answer to "FSR support".
3. **Both (chosen).** TAAU stays the default and the reference; FSR 3.1 is an option per
   view with the same inputs.

**How FSR is built:**

- a. The upstream CMake through the `cmake` crate: needs CMake, the Vulkan SDK and the
  prebuilt `FidelityFX_SC.exe` on every machine, builds a DLL, recompiles thousands of
  shader permutations. Rejected.
- b. The prebuilt signed `amd_fidelityfx_vk.dll`: a binary we would ship, against
  CLAUDE.md §0 and §6. Rejected.
- c. **`cc` over the SDK sources the upscaler needs, and our own generator for its SPIR-V
  (chosen).** Nine C++ files (seven of the SDK's, unmodified, and a shim) in one static
  library, cached by content hash; the Vulkan headers vendored. The shader permutations
  the engine uses are generated once by `examples/ffx_shaders.rs` and committed: byte-
  identical to the SDK's own tool's output (MD5), with the same reflection.

**Which SDK interface:**

- the ffx-api (`ffxCreateContext`, `ffxDispatch`): versioned, meant for the DLL; linking it
  links every provider, frame generation included. Not taken.
- **the effect's own API** (`ffxFsr3UpscalerContextCreate`/`Dispatch`), which ffx-api's
  upscaling provider wraps (chosen), behind a C shim of our own.

**Frame generation:** now, with the SDK's Vulkan frame interpolation swapchain, or
**later (chosen)**: it needs a present path the engine does not have yet (below).

**Texture LOD bias while upscaling** (both upscalers need it; TAAU had none):

- a per-sampler bias in the bindless samplers: global, not per view. Not taken.
- **a per-view bias in the forward renderer's material sampling (chosen)**:
  `Temporal::set_texture_lod_bias`, passed in the frame constants (the unused `jitter.z`),
  applied with `SampleBias` to every material sample, the alpha tests of the depth and
  shadow passes included, so the prepass and the shading pass keep the same pixels.

## Measurements

ref-1, release, GPU µs from the render graph's pass timestamps (the `taau` and `fsr`
passes, the same method for both). `cargo run -p engine_fsr --release --example
upscale_bench`: 576 cubes, camera circling (about a pixel a frame), HDR, shadow map 2048².
RTX: median of three runs; Radeon: the second of two runs, which agree within 3%.

**GPU time:**

| output ← render | RTX forward | RTX TAAU | RTX FSR | RTX FSR + RCAS | Radeon forward | Radeon TAAU | Radeon FSR | Radeon FSR + RCAS |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| 1920×1080 ← 1920×1080 | 152 | 149 | 787 | 828 | 3,929 | 3,860 | 15,558 | 16,431 |
| 1920×1080 ← 1280×720 (Quality) | 73 | 121 | 533 | 585 | 1,933 | 3,860 | 10,941 | 11,809 |
| 1920×1080 ← 960×540 (Performance) | 46 | 120 | 442 | 495 | 1,331 | 3,860 | 9,276 | 10,152 |
| 2560×1440 ← 2560×1440 | 261 | 253 | 1,406 | 1,490 | 6,665 | 6,862 | 27,667 | 29,373 |
| 2560×1440 ← 1706×960 (Quality) | 122 | 214 | 955 | 1,047 | 3,207 | 6,862 | 19,315 | 20,862 |
| 2560×1440 ← 1280×720 (Performance) | 74 | 211 | 926 | 981 | 1,940 | 6,860 | 16,408 | 17,964 |

- FSR costs 4.4× TAAU's resolve at 1080p from 720p on the RTX and 2.8× on the Radeon.
  Its cost grows with both the render and the output size; TAAU's with the output only.
- In this light scene (a forward pass of 0.07-0.26 ms on the RTX) FSR does not pay for
  itself: forward + FSR from 720p is 0.61 ms against 0.30 ms for native 1080p with TAA
  (Radeon 12.9 against 7.8 ms), while TAAU from 720p does (0.19 ms; Radeon 5.8 ms). FSR
  pays where the frame's render-size work saved is larger than FSR.
- 16-bit shaders (now enabled where supported): 2.5% faster on both GPUs (533 against 547
  µs on the RTX, 10.9 against 11.2 ms on the Radeon).
- The SDK's context: 134 MiB of GPU memory at 1440p from Quality (RTX; 126 MiB Radeon);
  creating one and drawing the first frame 11-14 ms (RTX), 55-69 ms (Radeon): 11 pipelines
  created without a pipeline cache.
- CPU, recording the frame's commands: about 20-30 µs with TAAU, 100-110 µs with FSR (image
  views and descriptor updates per dispatch).

**Image quality**, PSNR of tonemapped colors against 2×2 supersampling at 1920×1080, after
64 frames (same benchmark; one sample per pixel: 39.31 dB still):

| render | RTX TAAU still / moving | RTX FSR | RTX FSR + RCAS 0.5 | Radeon TAAU | Radeon FSR |
|---|---|---|---|---|---|
| 1920×1080 | 44.66 / 39.24 | 45.54 / 40.82 | 45.63 / 40.93 | 44.64 / 39.18 | 45.42 / 40.80 |
| 1280×720 | 42.64 / 36.94 | 43.74 / 38.56 | 43.53 / 38.90 | 42.62 / 36.95 | 43.66 / 38.52 |
| 960×540 | 40.38 / 34.89 | 41.73 / 36.99 | 42.11 / 37.03 | 40.42 / 34.89 | 41.66 / 36.96 |

FSR is 0.9-1.4 dB better with the camera still and 1.6-2.1 dB better moving. In
`tests/fsr.rs` (96×96 against 4×4 supersampling, synchronization validation on) the gap is
larger: native 43.2 against 39.6 dB, from half the size 36.9 against 33.0, panning 39.7
against 37.1 (RTX; Radeon within 0.2 dB).

**Texture LOD bias**, `tests/fsr.rs`, a 256² texture of 2×2-texel cells on the ground at a
grazing angle, from half the size, PSNR against 4×4 supersampling:

| bias | RTX TAAU | RTX FSR | Radeon TAAU | Radeon FSR |
|---|---:|---:|---:|---:|
| 0 | 26.25 | 27.59 | 26.38 | 27.56 |
| log2(render / output) = −1 | **26.43** | **27.91** | **26.48** | **27.79** |
| AMD's recommendation, −1 more = −2 | 26.35 | 27.70 | 26.38 | 27.56 |

The standard bias helps both upscalers; the extra level AMD recommends is worse on this
(deliberately high-frequency) texture. Real content may differ: measure on a textured
scene before changing the recommendation.

**Correctness**, both GPUs, synchronization validation, 0 errors and 0 warnings; the SDK's
own debug checking reports nothing:
- FSR antialiases, upsamples and follows motion (the thresholds of the TAAU test);
- the engine's jitter sequence is the SDK's, frame by frame, and the phase count and the
  quality modes' ratios are the SDK's;
- a change of output size creates a new context; sharpening runs;
- breaking the motion vectors' sign loses 5.5 dB panning, the jitter's sign 6 dB
  upscaled: the test would catch a convention error.

## Decision

- **Both upscalers.** TAAU stays the default and the reference; FSR 3.1 is chosen per
  view. How a game chooses (settings menu, per platform) is for the team (questions in
  the pull request).
- **Crate `engine_fsr`** beside `engine_upscale`, so the C++ toolchain and the SDK stay out
  of the rest:
  - `Fsr::new(&device, frames_in_flight)`; `add_pass(fg, FsrInput, output_size,
    frame_number) -> Texture` (RGBA16F at the output size, for tonemapping), the shape of
    `Taau::add_pass`. `FsrInput` is `TaauInput` plus the camera's planes and vertical field
    of view (`FsrCamera`) and the frame time.
  - `QualityMode` (Native AA, Quality, Balanced, Performance, Ultra Performance) with
    `render_size(output)`; `Fsr::jitter_phases`; `set_sharpness(Option<f32>)` (RCAS);
    `reset()` (and `MotionOutput::reset` from `Temporal::reset`).
  - One graph pass, `fsr`: it declares the inputs as compute-sampled and the output as a
    storage write, transitions the inputs to the layout the SDK expects and back, and lets
    the SDK record its own passes and barriers.
  - The SDK's context is created at the first frame and again when the output size
    changes or the render size grows; the old one is destroyed after the frames in
    flight. Flags: HDR, automatic exposure, dynamic resolution; motion vectors at the
    render size, standard depth.
  - Frame time and camera parameters are the caller's.
- **Build (option c):** `third_party/FidelityFX-SDK` (only the files used) and
  `third_party/Vulkan-Headers`; `build.rs` compiles them with `cc`; `csrc/shim.cpp` is the
  C interface, forwards the backend's direct Vulkan calls through the engine's loader and
  creates the shared images; `csrc/blobs.cpp` replaces the SDK's blob accessor. The SPIR-V
  and its tables are regenerated by `cargo run -p engine_fsr --example ffx_shaders --
  --glslang <the SDK's glslangValidator.exe>`, with one fix: `rw_luma_history`'s declared
  format (`rgba8`) becomes the bound image's (`rgba16f`).
- **`engine_rhi`** enables `shaderFloat16` and `shaderInt16` where supported
  (`OptionalCaps::shader_float16`, `Device::shader_float16_enabled`) and exposes
  `Device::entry` for libraries that load Vulkan themselves.
- **Texture LOD bias:** `Temporal::set_texture_lod_bias` (default 0),
  `engine_upscale::texture_lod_bias(render, output)` = log2(render / output), recommended
  for both upscalers; `Fsr::texture_lod_bias` is AMD's recommendation (one level more).
- **Frame generation is deferred.** On Vulkan it needs the SDK's replacement swapchain
  and its functions, a present queue and an image acquire queue the engine never uses,
  two pacing threads, a hudless image and the UI in its own target, optical flow and
  frame interpolation (two more shader sets), and pacing and latency aware of generated
  frames. The engine has none of the present-path pieces yet (`frame-generation.md`).
  Prerequisites, in order: a pluggable presenter in `engine_rhi`, spare queues, the UI in
  its own target with a hudless image, FG-aware `engine_latency`.
- **The `Upscaler` trait** ADR 0043 planned for the second implementation is not written
  yet: TAAU needs the allocator and bindless set at `add_pass`, FSR the camera and the
  frame time, and DLSS (next, through Streamline) will add its own. It is written with
  DLSS, over `FsrInput`'s superset of inputs.

## Consequences

- Games get FSR 3.1 on any Vulkan 1.3 GPU with no DLL and no SDK installed; building the
  engine needs only the C++ compiler it already needs for Jolt.
- The SDK's per-frame CPU work (image views, descriptor updates, and heap allocations for
  debug labels when debug utils are on) happens in C++ the engine's allocation test does
  not see; FSR's recording costs about 75 µs more CPU than TAAU's.
- Context creation (a new output size, the first frame) is a hitch of 11-69 ms: the SDK
  creates its pipelines without a pipeline cache. Revisit if resizing in play matters
  (create the context ahead, or on a worker thread).
- Updating the SDK: replace the vendored files, rerun the generator with the SDK's
  glslang, run `tests/fsr.rs` on both GPUs. A Vulkan release of the FSR 2.x line (FSR 4)
  would replace this integration.
- Not done:
  - frame generation (above);
  - the engine's exposure as FSR's exposure input (a 1 × 1 texture copied from the
    exposure buffer); FSR computes its own for now;
  - a transparency and composition mask (reflections, UV animation);
  - a settings-level choice of upscaler and quality mode (the editor's viewport uses
    neither yet);
  - the texture LOD bias on real content (Sponza) to settle AMD's extra level.
