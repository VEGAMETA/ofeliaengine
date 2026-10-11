# Visibility buffer and meshlets

Research for queue item 4 ("Visibility buffer / meshlets. Write the ADR first, with no
persistent per-triangle visibility bits"), 2026-10-11. Decision pending:
[ADR 0085](../adr/0085-visibility-buffer-and-meshlets.md) (Proposed). Earlier notes:
[gpu-driven-rendering.md](gpu-driven-rendering.md), [mesh-lods.md](mesh-lods.md).

## What the renderer does today

Clustered forward+ (ADR 0045), GPU-driven (ADR 0034): instances culled on the GPU (frustum,
two-phase HiZ occlusion, ADR 0035), one indirect draw per pipeline state, detail levels per
instance (ADR 0053), an optional depth prepass (with AO and on the iGPU), MSAA 2–8×
(ADR 0075). Shading features are literal parameters of the fragment shader, one entry point
per combination (32 today; the register rule). On the Radeon iGPU the forward pass is most
of the frame (Sponza 1080p: 12 of 18 ms).

## Sources (checked 2026-10-11)

| Source | License | How | Notes |
|---|---|---|---|
| Burns and Hunt, "The Visibility Buffer: A Cache-Friendly Approach to Deferred Shading" (JCGT 2(2), 2013) | paper | Rasterize triangle and instance IDs only (4 bytes a pixel); shade in a full-screen pass that fetches the triangle's vertices and interpolates them | The idea |
| Schied and Dachsbacher, "Deferred Attribute Interpolation for Memory-Efficient Deferred Shading" (HPG 2015) | paper | Attributes interpolated from stored triangles; analytic derivatives | Derivatives without quads |
| Wihlidal, "Optimizing the Graphics Pipeline with Compute" (GDC 2016) | talk | Cluster (meshlet) culling, per-triangle culling and index compaction in compute before rasterizing | Geometry culling without mesh shaders |
| Engel, "The Filtered and Culled Visibility Buffer" (GDC Europe 2016); The Forge's Triangle Visibility Buffer (1.0 and 2.0) | Apache-2.0 | Clusters of 256 triangles culled by cones, triangles filtered in async compute into compacted index buffers, empty draws removed; IDs rasterized, shaded per pixel | Open implementation on Vulkan |
| Karis et al., "Nanite" (SIGGRAPH 2021 Advances) | talk (closed engine) | Meshlet hierarchies, 64-bit visibility buffer (depth and IDs) written by hardware raster or, for small triangles, by compute with 64-bit atomics; materials shaded by classifying tiles | The far end: software raster |
| Wicked Engine | MIT | Visibility buffer with compute shading of materials | A second open implementation |
| Bevy (meshlets, "virtual geometry") | MIT / Apache-2.0 | Nanite-like meshlet hierarchies, visibility buffer, software raster | Experimental |
| meshoptimizer | MIT | `meshopt_buildMeshlets`, cone and bounds per meshlet | Already a dependency (detail levels) |

No code was copied from any of them.

## What the GPUs offer (vulkaninfo, 2026-10-11)

- `VK_EXT_mesh_shader`: the RTX has it; the Radeon iGPU's driver (26.8.1) still has not.
  Meshlets must work through compute culling and indirect draws there.
- 64-bit image and buffer atomics (`shaderImageInt64Atomics`): both.

## Patents (search 2026-10-11)

- Microsoft US 10,672,182 B2 (active until 2038): a buffer of one visibility bit per
  primitive of each instance, consulted to render or skip it (ADR 0035's note). Culling
  must be decided afresh every frame; no persistent per-triangle (or per-meshlet-triangle)
  visibility bits.
- Intel US20130271465A1 / WO2013101150A1 ("Sort-based tiled deferred shading architecture
  for decoupled sampling"): a hardware architecture sorting shading points per tile
  on-chip. Status to check before a visibility buffer that sorts shading work by triangle.
- Texture-space shading patents (US12602856, US12664715: meshlet and triangle IDs packed
  to shade in texture space): not candidates; avoid texture-space shading.

## Conclusions

1. **Meshlets first**, inside the current renderer: clusters built at cook time
   (meshoptimizer), culled on the GPU per view (frustum, normal cone, the HiZ pyramid,
   per frame), drawn through compacted index buffers and indirect draws (the iGPU) or mesh
   shaders (the RTX). They cut vertex and raster work in dense scenes and are what any
   visibility buffer needs anyway.
2. **A visibility buffer** replaces the opaque shading: IDs rasterized, materials shaded in
   compute by tile classes. It changes how shading permutations work (the register rule,
   ADR 0058) and how custom materials plug in (queue item 3b, "after GI"), and MSAA (ADR
   0075) needs a per-sample path or stays forward. Decide it together with 3b's ADR, with
   measurements of meshlets in hand.
3. **Software rasterization of small triangles** (64-bit atomics) only after 2, if dense
   geometry is the bottleneck.
