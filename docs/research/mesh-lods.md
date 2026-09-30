# Mesh detail levels

Research for LODs (the team's roadmap: LODs, then impostors as the farthest level).
Decision: [ADR 0053](../adr/0053-mesh-lods.md).

Checked on 2026-09-30. Engines are described from their public documentation; no code was
read or copied (meshoptimizer is used as a library).

## Sources

| Source | Version | License | What we take |
|---|---|---|---|
| meshoptimizer (Arseny Kapoulkine), through `meshopt` | 0.25 (in `meshopt` 0.6.2) | MIT (crate MIT OR Apache-2.0) | `simplify` (quadric error edge collapses, returns the geometric error), `simplifyScale`, `optimizeVertexCache` |
| M. Garland, P. Heckbert, "Surface Simplification Using Quadric Error Metrics", SIGGRAPH 1997 | — | paper | the error measure behind the simplifier |
| Unreal Engine (static mesh LODs, screen size), Unity (LOD Group) | public docs | products | levels by screen size or projected error, per-instance selection |
| GPU-driven rendering (Haar, Aaltonen, "GPU-Driven Rendering Pipelines", SIGGRAPH 2015) | — | talk | selecting levels in the culling compute pass |

## Choosing a level

- **Screen size bands** (Unity's LOD Group, Unreal's screen size): simple, but each asset
  needs tuning, and the right band depends on resolution and field of view.
- **Projected error**: the simplifier knows how far each level strays from the full mesh;
  projecting that distance to pixels at the instance's distance gives a criterion that
  needs no tuning and follows resolution and FOV.
  - pixels per unit at distance 1 = the projection's vertical scale × half the height;
  - scale: the largest axis scale of the instance;
  - distance: the box center's minus the box radius (never a coarser level too close).

## Where to choose

- On the CPU per instance: costs per instance per frame and splits batches.
- In the culling compute shader: the instance's box and transform are already loaded; one
  more test and a different command to append to. Each level needs its own indirect
  command and its own range of instance slots.

## Measured

See ADR 0053: 6–9× less forward shading for a field of 16k-triangle spheres on both GPUs;
Sponza's image changes by PSNR 46.5 dB.

## Open questions

- Impostors (the next step): octahedral view atlases for the farthest level.
- Cross-fading between levels (dithered).
- Attribute-aware simplification (`simplifyWithAttributes`) for hard edges and UV seams.
- Levels in the derived data cache.
