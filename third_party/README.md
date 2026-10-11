# Third-party sources

Source only, each with its license (CLAUDE.md §2, §15), unmodified but for the changes
under "Local modifications". Every entry is also listed in `THIRD_PARTY_NOTICES.md`.

| Directory | Project | Version | License | Used by |
|---|---|---|---|---|
| `JoltPhysics/` (only `Jolt/` and `LICENSE`) | [Jolt Physics](https://github.com/jrouwe/JoltPhysics) | v5.6.0, commit `e77f175595e64cb44218cc9d9d56fc365ad0e36a` | MIT | `engine_physics_jolt` |
| `box2d/` (only `include/`, `src/` without `CMakeLists.txt`, `LICENSE`) | [Box2D](https://github.com/erincatto/box2d) | v3.1.1, commit `8c661469c9507d3ad6fbd2fea3f1aa71669c2fe3` | MIT | `engine_physics2d` |
| `joltc/` (only `include/`, `src/`, `LICENSE`) | [JoltC](https://github.com/amerkoleci/joltc) | commit `886e088675bae3a086f8318c7803f8ee962c2f2c` (builds Jolt v5.6.0) | MIT | `engine_physics_jolt` |
| `FidelityFX-SDK/` (only `LICENSE.txt`, `sdk/LICENSE.txt` and the files of the FSR 3.1 upscaler, frame interpolation, optical flow, Brixelizer, Brixelizer GI, the Vulkan backend and their shared code, headers and GLSL; not the frame interpolation swapchain) | [AMD FidelityFX SDK](https://github.com/GPUOpen-LibrariesAndSDKs/FidelityFX-SDK) | v1.1.4, commit `c6efa6bf7f2027b3ec94f28578bb5965eabb9e55` | MIT | `engine_ffx` (built for `engine_fsr`: upscaling, ADR 0060, and frame generation's interpolation, ADR 0091; and Brixelizer GI, ADR 0090); `engine_render`'s FSR 1 (`shaders/fsr1.slang` ports `sdk/include/FidelityFX/gpu/fsr1/ffx_fsr1.h`, ADR 0075) |
| `smaa/` (only `SMAA.hlsl` and `LICENSE.txt`) | [SMAA](https://github.com/iryoku/smaa) reference implementation | commit `71c806a838bdd7d517df19192a20f0c61b3ca29d` | MIT | `engine_render` (SMAA and MLAA after tone mapping, ADR 0075; included by `crates/engine_render/shaders/smaa_port.hlsli`; its area and search tables are `crates/engine_render/data/smaa_*.bin`, the bytes of the same commit's `Textures/AreaTex.h` and `SearchTex.h`) |
| `Vulkan-Headers/` (only `include/vulkan/{vulkan.h,vulkan_core.h,vk_platform.h}`, `include/vk_video/`, `LICENSE.md`, `LICENSES/`) | [Vulkan-Headers](https://github.com/KhronosGroup/Vulkan-Headers) | v1.4.350, commit `8864cdc896bbc2a9b6eb36b3218fc9ef57908d77` | Apache-2.0 OR MIT | `engine_ffx` (the FidelityFX SDK's Vulkan backend) |

To update, replace the directory with the new version's files, record the version and
commit here, and run the `engine_physics_jolt` tests: the FFI layout test catches struct
changes. For the FidelityFX SDK, also regenerate the shaders
(`cargo run -p engine_ffx --example ffx_shaders -- --glslang <the SDK's
sdk/tools/binary_store/glslangValidator.exe>`) and run the `engine_ffx` and `engine_fsr`
tests on both GPUs (ADR 0060, ADR 0090). Carry the local modifications over, or drop those
the new version fixes.

## Local modifications

### FidelityFX SDK: Brixelizer on AMD's Vulkan compiler (ADR 0090)

Three changes in `sdk/include/FidelityFX/gpu/brixelizer/`, each marked "an ofeliaengine
fix" in place. Found on ref-1's Radeon (RDNA2, driver 26.8.1), which built a test scene's
distance field with holes through its floor and wall: 30 054 bricks where the RTX builds
38 468. With them, both GPUs build the same bricks.

1. **Edges from the world-space triangle** (`ffx_brixelizer_cascade_ops.h`,
   `ffx_brixelizer_common_private.h`). `FfxBrixelizerGet2DEdges` took the triangle's edges
   as differences of its vertices in brick coordinates. A triangle's projection onto a
   plane it stands perpendicular to (a floor's onto xy) is a segment, whose conservative
   edge tests pass with no margin: an edge parallel to an axis must be exactly zero along
   it. AMD's compiler leaves residues below the coordinates' own precision instead (1e-5
   between two equal coordinates of 1624.5: the subtraction fused with the division into
   an FMA), the tests fail and whole triangles are left out of their bricks. `precise` on
   the subtraction does not help: AMD's driver merges it with `EmitSDF`'s undecorated twin
   (`ba = V1 - V0`) and fuses it anyway (`engine_rhi`'s `contraction_repro` example;
   reported to AMD, FidelityFX SDK issue #202). `FfxBrixelizerEmitSDF`
   now passes the edges in, from the world-space vertices, where equal coordinates subtract
   to zero. `engine_brixelizer`'s `the_distance_field_has_no_holes` fails without it.
2. **Edges stored as the nearest halves** (`ffx_brixelizer_cascade_ops.h`,
   `FfxBrixelizerStoreTrianglePartial`). The rounding of the half conversion is the
   implementation's; AMD's truncates (an edge of 39.999996 is stored as 39.97, not 40), so
   the vendors' distance fields differed. The edges are rounded to the nearest half first.
3. **Device-scope fences around the scans' last-group handoffs**
   (`ffx_brixelizer_cascade_ops.h`, `FfxBrixelizerScanJobs`, `FfxBrixelizerScanReferences`):
   each group's first thread writes the group's sum and counts its arrival, and the last
   group reads every sum. A group barrier orders neither across groups; the memory model
   needs a release and an acquire. Not seen to fail; two fences per group.
