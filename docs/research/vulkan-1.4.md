# Vulkan 1.4: what it adds, who has it, what it gives this engine

Checked 2026-09-27. Sources at the end.

## What 1.4 made core

Promoted extensions: `VK_KHR_dynamic_rendering_local_read`, `VK_KHR_global_priority`,
`VK_KHR_index_type_uint8`, `VK_KHR_line_rasterization`, `VK_KHR_load_store_op_none`,
`VK_KHR_maintenance5`, `VK_KHR_maintenance6`, `VK_KHR_map_memory2`, `VK_KHR_push_descriptor`,
`VK_KHR_shader_expect_assume`, `VK_KHR_shader_float_controls2`,
`VK_KHR_shader_subgroup_rotate`, `VK_KHR_vertex_attribute_divisor`, `VK_EXT_host_image_copy`
(the feature stays optional), `VK_EXT_pipeline_protected_access`, `VK_EXT_pipeline_robustness`.
Push descriptors, local read (at least for storage resources and single-sampled color
attachments) and scalar block layout became mandatory, along with higher minimum limits.

## Who has 1.4 drivers on Windows

| Vendor | 1.4 | Stays at 1.3 |
|---|---|---|
| NVIDIA | Maxwell and newer (Maxwell, Pascal and Volta frozen at branch 580) | Kepler and older (already below our floor) |
| AMD | RDNA 1–4 | Polaris and Vega on maintenance drivers, **including every Ryzen APU with Vega graphics (2000–5000 series)** |
| Intel | Arc, Xe-LPG and newer (Meteor Lake, Arrow Lake) | 11th–14th generation processor graphics (Iris Xe, UHD), on legacy drivers since 2025-09; Gen9 |

A 1.4 floor would drop the Vega APUs and the Iris Xe laptops: a large share of the
integrated GPUs that ADR 0006 set out to reach.

## ref-1 (2026-09-27)

| | RTX 4060 Ti, driver 616.92, Vulkan 1.4.351 | Radeon iGPU, driver 25.10.36, Vulkan 1.4.315 |
|---|---|---|
| maintenance5–8, push descriptor, local read, load/store op none, maximal reconvergence, line rasterization, uint8 indices, HDR metadata, pipeline library, pipeline binary, descriptor buffer | yes | yes |
| `VK_EXT_host_image_copy` | yes | **no** |
| present wait / present id | yes | **no** |
| mesh shaders | yes | **no** |

## Bindings

`ash` 0.38.0+1.3.281 is still the latest release on crates.io: no `VkPhysicalDeviceVulkan14Features`,
no `API_VERSION_1_4`. Git dependencies are denied (ADR 0006). The 1.4 capabilities are
therefore used as the extensions they were promoted from, which 1.4 drivers keep listing.

## What each one would give this engine today

| Capability | Today | When it pays |
|---|---|---|
| Push descriptors | nothing: one bindless set, bound once per pass | — |
| Local read | nothing: forward rendering, no G-buffer | visibility buffer / deferred resolve in the same render pass |
| maintenance5 (pipelines without shader modules) | pipeline creation is 2–5 ms warm on both GPUs (the drivers' disk caches); a module-free path next to the 1.3 one adds code instead of removing it | many shader variants compiled and hot-reloaded at once |
| load/store op none | nothing: the graph already stores only attachments read later (depth `DontCare` after forward unless the pyramid reads it) | read-only depth passes (transparents, decals) |
| Host image copy | nothing measurable: the glyph atlas uploads once | texture streaming from IoRing without staging (NVIDIA only on ref-1) |
| Maximal reconvergence | correctness insurance for the wave-aggregated atomics in GPU culling | when a vendor's compiler reorders wave operations after divergence |
| Line rasterization | — | the debug draw API (wide, smooth lines) |
| HDR metadata | — | HDR output (next) |

## Conclusion

- Keep the 1.3 floor.
- Detect the 1.4 capabilities and report them (`OptionalCaps`); features use them when they
  measurably help, with the 1.3 path kept.
- No speed-up is available from 1.4 in the current code: none of the capabilities touches a
  measured cost.

## Sources

- [Vulkan 1.4 feature proposal (docs.vulkan.org)](https://docs.vulkan.org/features/latest/features/proposals/VK_VERSION_1_4.html)
- [Khronos press release, Vulkan 1.4](https://www.khronos.org/news/press/khronos-streamlines-development-and-deployment-of-gpu-accelerated-applications-with-vulkan-1.4)
- [Phoronix: Vulkan 1.4 released](https://www.phoronix.com/news/Vulkan-1.4-Released)
- [The FPS Review: Vulkan 1.4 drivers from NVIDIA, AMD and Intel](https://www.thefpsreview.com/2024/12/02/vulkan-1-4-graphics-api-support-coming-to-amd-and-intel-gpus-early-next-year-as-nvidia-begins-shipping-conformant-drivers/)
- [AMD Software PRO Edition 25.Q2 release notes (Vulkan 1.4 on RDNA; Polaris/Vega maintenance)](https://www.amd.com/en/resources/support-articles/release-notes/RN-PRO-WIN-25-Q2.html)
- [Intel: driver support update for 11th–14th generation processor graphics](https://www.intel.com/content/www/us/en/support/articles/000101986/graphics.html)
- [Intel graphics driver 32.0.101.6557 (Vulkan 1.4.303)](https://www.geeks3d.com/20250130/intel-graphics-driver-32-0-101-6557-6262-download/)
- ref-1: `vulkaninfoSDK --json` from the Vulkan SDK 1.4.350, and `engine_rhi` `tests/gpu.rs`.
