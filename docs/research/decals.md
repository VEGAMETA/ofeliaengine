# Decals

Research for queue item 2 (clustered decals), 2026-10-11: posters, blood, scorch marks,
bullet holes, dirt and road paint, projected onto whatever is under them. Decision
pending: [ADR 0082](../adr/0082-decals.md) (Proposed).

## Sources (checked 2026-10-11)

| Source | License | How | Notes |
|---|---|---|---|
| id Tech 6 (DOOM 2016): Sousa and Geffroy, "The devil is in the details: idTech 666", SIGGRAPH 2016 Advances | talk | Clustered forward: decals binned in the same clusters as lights and cubemaps (up to 256 each per cluster), textures in an atlas, applied while a mesh is shaded | The reference design for a clustered forward renderer |
| Godot 4 ([docs](https://docs.godotengine.org/en/stable/tutorials/3d/using_decals.html)) | MIT | Clustered decals in Forward+: albedo, normal, ORM and emission maps in an atlas, applied when meshes draw; normal fade (by the angle between the projection and the surface), distance fade, upper and lower fade | The fades; their atlas we do not need (bindless) |
| Bevy 0.16 ([release notes](https://bevy.org/news/bevy-0-16/)) | MIT / Apache-2.0 | `ClusteredDecal`: a 1 × 1 × 1 box projecting along +Z, a clusterable object like lights and probes; needs bindless textures. Also forward decals: a mesh with the decal's material, blended | Bindless textures, as ours, instead of an atlas |
| Deferred decals (Killzone 2, Frostbite and others; Unreal's DBuffer decals in forward) | various | Decal boxes rasterized after the depth prepass into decal buffers that the material reads before lighting | One evaluation per pixel; needs the prepass; opaque surfaces only |

## What it means for our forward renderer

- Lights are binned by z-bins and tile bitmasks (ADR 0045). Decals fit the same structure:
  sorted by depth, a bit per decal per tile, the shader walks the bits.
- **The register rule** (CLAUDE.md §5, ADR 0058): code for decals must not exist in the
  shading of views without them. The shading's features are literal parameters of
  `shade_surface`, one entry point per combination: 32 fragment entry points today
  (lights × environment × ambient occlusion × local shadows × contact shadows ×
  reflections × subsurface × masked, motion, blended...). Clustered decals in the shading
  double them, and forward.rs's sets with them (lazy since ADR 0079: no start-up cost).
- A decal buffer before lighting keeps one decal evaluation per pixel out of the
  shading: the shading reads one or two texels (still a variant, a small one). It needs
  the depth prepass (on whenever AO is, and in the iGPU defaults) and leaves transparent
  surfaces undecaled.

Patents: none found for clustered decals or deferred decal buffers in a quick search
(2026-10-11); a proper search comes with the decision.
