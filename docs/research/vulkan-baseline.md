# Vulkan baseline, device selection and GPU testing: research

Date: 2026-09-25. Input for [ADR 0006](../adr/0006-vulkan-baseline-and-device.md). No code was
copied from the projects below.

## Sources

| Source | Version checked | License | What we looked at |
|---|---|---|---|
| [Wicked Engine](https://github.com/turanszkij/WickedEngine) `wiGraphicsDevice_Vulkan.cpp` | commit `2ff1d9e` (2026-09-24) | MIT | Vulkan 1.3 instance, device choice, required features |
| [ash](https://github.com/ash-rs/ash) | 0.38.0+1.3.281 (latest on crates.io, 2026-09-25) | MIT OR Apache-2.0 | bindings |
| Mesa lavapipe | 25.2.8 (Ubuntu 24.04 package), Vulkan 1.4.318 | MIT | software Vulkan for container tests |
| Khronos validation layer | 1.3.275 (container), 1.4.350 (ref-1, Vulkan SDK) | Apache-2.0 | correctness checking in tests |
| Slang | 2026.12 (`libslang-compiler` from the slangpy 0.43.1 wheel) | Apache-2.0 WITH LLVM-exception | bootstrap shader compilation |

## Wicked Engine

- Requests `VK_API_VERSION_1_3`. Chains `VkPhysicalDeviceVulkan11/12/13Features` and requires
  `dynamicRendering`, plus the full descriptor-indexing set for bindless (update-after-bind for
  sampled images, storage images and storage buffers, partially bound, variable count,
  runtime arrays, non-uniform indexing).
- Device choice takes the first discrete GPU, or the first device matching a user preference
  (integrated, or a vendor-name match), and otherwise the first suitable device.

What we take:
- The same required feature set, extended with `synchronization2`, `timelineSemaphore`,
  `bufferDeviceAddress`, `maintenance4`, `scalarBlockLayout` and `shaderDrawParameters`, which
  the engine's frame and bindless design rely on (§6).
- Scoring by device kind, with **device-local memory size** as the tie-breaker. With several
  discrete GPUs, the larger one wins.
- A preference override, `ENGINE_ADAPTER` (`high-performance`, `low-power`, or a name substring).
  It lets ref-1 test its AMD iGPU as well as its NVIDIA dGPU.

## ash version gap

ash 0.38 ships Vulkan 1.3.281 headers. Newer extensions the engine plans to use are missing:
`VK_AMD_anti_lag` (1.3.289), `VK_KHR_pipeline_binary` (1.3.294), and Vulkan 1.4 core. For now:
- availability is detected by comparing **extension names** as strings;
- when a feature is implemented, the few structs and function pointers it needs will be declared
  in `engine_rhi` (plain `#[repr(C)]` types plus `vkGetDeviceProcAddr`);
- we will switch to a newer ash release, or generate bindings from `vk.xml`, if the gap grows. That
  needs a separate ADR.

## Testing without and with GPUs

- **Container (Linux):** Mesa lavapipe provides Vulkan 1.4 on the CPU, with swapchain, mesh
  shaders, ray tracing, descriptor buffer and GPL. The whole RHI can be exercised offscreen, with
  the Khronos validation layer active.
- **Hosted CI (Windows, no GPU):** GPU tests print a skip note unless `ENGINE_REQUIRE_GPU=1`.
- **GPU workflow (ref-1):** `ENGINE_REQUIRE_GPU=1`, validation from the Vulkan SDK, run twice: on
  the default (NVIDIA RTX 4060 Ti) and on `ENGINE_ADAPTER=radeon` (AMD iGPU).
- **Validation errors fail tests.** The messenger counts them. A mutation test (removing the layout
  transition barrier) is reported as `InvalidImageLayout` and fails the test.

## Slang bootstrap

`slangc` is not in the container, but `libslang-compiler` (Slang 2026.12) from the slangpy wheel
exports the C API (`spCreateSession`, `spProcessCommandLineArguments`, `spCompile`,
`spGetEntryPointCode`), which accepts `slangc` arguments. This produced the triangle's SPIR-V 1.6.
On Windows the Vulkan SDK ships `slangc.exe` and `slang.dll`. The shader pipeline (`engine_shader`,
§9) will load the Slang library at run time.
