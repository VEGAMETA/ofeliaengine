# Transparency

Research for blended transparency (the team's first priority, 2026-09-30).
Decision: [ADR 0054](../adr/0054-transparency.md).

Checked on 2026-09-30. Engines and FSR are described from their public documentation and
papers; no code was read or copied.

## Sources

| Source | Version | License | What we take |
|---|---|---|---|
| T. Porter, T. Duff, "Compositing Digital Images", SIGGRAPH 1984 | — | paper | the "over" operator on premultiplied colors |
| glTF 2.0 specification, `alphaMode` | 2.0 | spec | `BLEND`: base color alpha is coverage |
| M. McGuire, L. Bavoil, "Weighted Blended Order-Independent Transparency", JCGT 2013 | — | paper | the OIT alternative and its limits |
| L. Bavoil, K. Myers, "Order Independent Transparency with Dual Depth Peeling", NVIDIA 2008 | — | paper | depth peeling's cost |
| J. Yang et al., "Real-Time Concurrent Linked List Construction on the GPU", EGSR 2010 | — | paper | per-pixel lists |
| E. Enderton et al., "Stochastic Transparency", I3D 2010 | — | paper | coverage-mask transparency |
| AMD FidelityFX Super Resolution 2, reactive mask | public docs | MIT (docs read only) | lowering temporal history where transparent surfaces cover |
| Unreal Engine (translucency sort, separate translucency), Unity (transparent queue) | public docs | products | sorting per object by distance to bounds, the options for TAA |

## Ordering

"Over" is not commutative: `a over (b over c)` differs from `b over (a over c)`.

- **Sort per object, back to front.** What Unity's transparent queue and Unreal's
  translucency do by default.
  - Exact for layers that do not intersect.
  - Wrong inside one object (triangles in index order) and where objects interpenetrate.
  - Sort keys: distance from the camera to the bounds center. Unreal offers a manual
    priority on top.
- **Weighted blended OIT:** accumulate `Σ w·c·α` and `Π(1 − α)` and divide.
  - Order independent in one pass, but the weight function stands in for the order.
  - Visibly wrong for layers above about α 0.7 and when the depth range is large.
- **Exact OIT** (linked lists, k-buffers, depth peeling): memory or passes proportional
  to the number of layers. Fine for a few objects on discrete GPUs, not a default for an
  iGPU at 1080p.
- **Stochastic transparency:** random coverage masks averaged by MSAA or TAA; noisy until
  the temporal filter converges.

Per-object sorting is the baseline every engine ships. OIT pays where layers intersect
heavily (particles, hair, foliage cards), which are specialized systems anyway.

## GPU-driven drawing and order

With GPU culling (ADR 0034) the culling threads append visible instances to their batch's
command in an unspecified order, so a sort done on the CPU is lost within a batch. Either:

- each blended instance is its own batch and command (the order is the order of the
  commands, which culling does not change); or
- the visible blended instances are sorted on the GPU after culling (radix sort by depth
  key), then drawn from one list.

The first costs per instance on the CPU and in command processing, and needs no new pass.
The second scales to many thousands and suits particles.

## Temporal upsampling

A transparent surface does not write motion: the pixel's motion vector is the opaque
surface's behind it. Reprojecting history along it drags the transparent layer's old
image (ghosting over a moving background, smearing of a moving glass).

- FSR 2 takes a **reactive mask**: a per-pixel value, typically the alpha of transparent
  surfaces, that reduces the history's contribution.
- Unreal can render translucency **after** the temporal upscale ("separate translucency"),
  at the cost of antialiasing and full-resolution shading.

Our resolve keeps an accumulated weight per pixel, capped (16 still, 4 moving): the mask
lowers the cap towards one frame's weight as coverage rises. Accumulating the mask with
premultiplied blending gives the layers' combined coverage `1 − Π(1 − α)` without another
pass.

## Measured

See ADR 0054. 10,000 blended cubes: add_passes about 450 µs on the CPU (37 ns per blended
instance); forward + transparent 92–127 µs on the RTX 4060 Ti, 2.1 ms on the Radeon iGPU.
