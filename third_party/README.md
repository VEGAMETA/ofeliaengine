# Third-party sources

Source only, unmodified, each with its license (CLAUDE.md §2, §15). Every entry is also
listed in `THIRD_PARTY_NOTICES.md`.

| Directory | Project | Version | License | Used by |
|---|---|---|---|---|
| `JoltPhysics/` (only `Jolt/` and `LICENSE`) | [Jolt Physics](https://github.com/jrouwe/JoltPhysics) | v5.6.0, commit `e77f175595e64cb44218cc9d9d56fc365ad0e36a` | MIT | `engine_physics_jolt` |
| `box2d/` (only `include/`, `src/` without `CMakeLists.txt`, `LICENSE`) | [Box2D](https://github.com/erincatto/box2d) | v3.1.1, commit `8c661469c9507d3ad6fbd2fea3f1aa71669c2fe3` | MIT | `engine_physics2d` |
| `joltc/` (only `include/`, `src/`, `LICENSE`) | [JoltC](https://github.com/amerkoleci/joltc) | commit `886e088675bae3a086f8318c7803f8ee962c2f2c` (builds Jolt v5.6.0) | MIT | `engine_physics_jolt` |
| `FidelityFX-SDK/` (only `LICENSE.txt`, `sdk/LICENSE.txt` and the files of the FSR 3.1 upscaler, Brixelizer, Brixelizer GI, the Vulkan backend and their shared code, headers and GLSL) | [AMD FidelityFX SDK](https://github.com/GPUOpen-LibrariesAndSDKs/FidelityFX-SDK) | v1.1.4, commit `c6efa6bf7f2027b3ec94f28578bb5965eabb9e55` | MIT | `engine_ffx` (built for `engine_fsr`, ADR 0060, and Brixelizer GI, ADR 0090); `engine_render`'s FSR 1 (`shaders/fsr1.slang` ports `sdk/include/FidelityFX/gpu/fsr1/ffx_fsr1.h`, ADR 0075) |
| `smaa/` (only `SMAA.hlsl` and `LICENSE.txt`) | [SMAA](https://github.com/iryoku/smaa) reference implementation | commit `71c806a838bdd7d517df19192a20f0c61b3ca29d` | MIT | `engine_render` (SMAA and MLAA after tone mapping, ADR 0075; included by `crates/engine_render/shaders/smaa_port.hlsli`; its area and search tables are `crates/engine_render/data/smaa_*.bin`, the bytes of the same commit's `Textures/AreaTex.h` and `SearchTex.h`) |
| `Vulkan-Headers/` (only `include/vulkan/{vulkan.h,vulkan_core.h,vk_platform.h}`, `include/vk_video/`, `LICENSE.md`, `LICENSES/`) | [Vulkan-Headers](https://github.com/KhronosGroup/Vulkan-Headers) | v1.4.350, commit `8864cdc896bbc2a9b6eb36b3218fc9ef57908d77` | Apache-2.0 OR MIT | `engine_ffx` (the FidelityFX SDK's Vulkan backend) |

To update, replace the directory with the new version's files, record the version and
commit here, and run the `engine_physics_jolt` tests: the FFI layout test catches struct
changes. For the FidelityFX SDK, also regenerate the shaders
(`cargo run -p engine_ffx --example ffx_shaders -- --glslang <the SDK's
sdk/tools/binary_store/glslangValidator.exe>`) and run the `engine_ffx` and `engine_fsr`
tests on both GPUs (ADR 0060, ADR 0090).
