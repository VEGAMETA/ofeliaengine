# 0085. Visibility buffer and meshlets: meshlets in the forward renderer first

- Status: Accepted (option 1 now, the owner, 2026-10-11)
- Date: 2026-10-11
- Research: [docs/research/visibility-buffer.md](../research/visibility-buffer.md)
- Related: [ADR 0034](0034-gpu-culling-and-indirect-draws.md) (GPU culling),
  [ADR 0035](0035-occlusion-culling.md) (two-phase HiZ; the per-triangle visibility bits
  patent), [ADR 0045](0045-local-lights.md) (clustered forward+), [ADR 0053](0053-mesh-lods.md)
  (detail levels), [ADR 0058](0058-ambient-occlusion.md) (the register rule), queue items 4
  and 3b

## Context

Queue item 4 asks for a visibility buffer and meshlets, with this ADR first and no
persistent per-triangle visibility bits (Microsoft US 10,672,182, ADR 0035). The renderer
is clustered forward+, culled per instance on the GPU, one indirect draw per pipeline
state, shading features as entry points of their own. The Radeon iGPU's forward pass is
two thirds of its frame; it has no mesh shaders (driver 26.8.1); both GPUs have 64-bit
image atomics.

## Options

1. **Meshlets in the forward renderer:**
   - clusters of up to 64 vertices and 124 triangles built at cook time (meshoptimizer,
     already a dependency), with their bounds and normal cones, per detail level;
   - culled per view every frame on the GPU (frustum, cone, the HiZ pyramid of ADR 0035;
     nothing kept from frame to frame per triangle);
   - drawn through a compacted index buffer and the existing indirect draws on GPUs without
     mesh shaders (the iGPU), through task and mesh shaders where they exist (the RTX);
   - the shading is untouched; shadows and probes reuse the cluster culling.
2. **A visibility buffer:** IDs (instance, meshlet, triangle) rasterized into 32 or 64
   bits, then materials shaded in compute per class of tiles with analytic derivatives;
   transparent surfaces stay forward. It replaces the forward opaque shading, changes how
   shading permutations and custom materials (item 3c) work, and needs a per-sample path
   for MSAA (or keeps MSAA on the forward path).
3. **Nanite-like software rasterization** of small triangles with 64-bit atomics into the
   visibility buffer: only on top of option 2.

## Recommendation (to confirm)

Option 1 now: it gains where geometry is dense without touching the shading, works on both
GPUs, and its clusters are what options 2 and 3 build on. Option 2 decided with item 3c's
ADR (custom materials decide the shading's shape) and with the meshlets' numbers on both
GPUs. Option 3 only if dense geometry is still the bottleneck then. A fresh patent check
comes with each step (the Intel tiled-deferred application in the research note before
option 2).

## Questions for the owner

1. Meshlets first (option 1), with the visibility buffer decided together with item 3c?
2. Or the visibility buffer now, before custom materials?

## Decision

**Option 1 now** (the owner, 2026-10-11): meshlets in the forward renderer, clusters built
at cook time and culled per view on the GPU, drawn through compacted indices where mesh
shaders are missing (the iGPU) and through task and mesh shaders where they exist. The
visibility buffer is decided together with custom materials (queue item 3c).

## Consequences

- The cook gains clusters per detail level (meshoptimizer), the pack a format addition.
- Culling per cluster on both GPUs; numbers on both before the visibility buffer's
  decision.
