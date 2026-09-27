# 0042. Depth prepass: optional, off by default

- Status: Accepted
- Date: 2026-09-27
- Builds on: [ADR 0040](0040-materials-and-textures.md) (materials, front-to-back
  batches), [ADR 0035](0035-occlusion-culling.md) (two-phase occlusion culling)

## Context

After materials, the forward pass carries the frame's cost. It samples textures and
evaluates the BRDF and 9 shadow taps per fragment, and a fragment hidden later by nearer
geometry is shaded for nothing.

ADR 0040 already draws batches front to back within each pipeline state, which halved
Sponza's forward pass. A depth prepass removes what overdraw remains:

1. a depth-only pass draws everything visible;
2. the forward pass then shades with the depth test set to *equal*, each pixel once.

Forward+ renderers commonly use one, and it gives depth-based effects (ambient occlusion,
clustered light bounds) depth before shading.

The equality test needs both passes to compute the same clip positions, bit for bit. A
driver fusing a multiply-add in one vertex shader and not in the other leaves holes.

## Options

1. **No prepass** (as before): front-to-back order only.
2. **Always a prepass.**
3. **A prepass per renderer, off by default (chosen),** measured on real scenes and on
   the cube benchmarks.

**Matching positions:**

- a. Rely on the driver compiling the same expression the same way in two shaders:
  common, but not guaranteed.
- b. **`precise` (chosen).**
  - Slang 2026.18.3 is the first release to emit SPIR-V `NoContraction` for it (#12198,
    upgraded in #60).
  - `mul(matrix, vector)` is a single instruction that cannot carry `NoContraction`, so
    `clip_position` writes both products out as sums. All 24 operations carry
    `NoContraction` in `vertexMain`, `depthVertex` and `depthVertexMasked`.

**Masked materials:**

- The prepass tests alpha (`depthVertexMasked` with the shadow pass's alpha-test fragment
  shader).
- The forward pass then shades every state with one pipeline that never discards.
  Fragments the test removed have no matching depth.

## Measurements

ref-1, release, 1920×1080 HDR, 2048² shadow map, GPU µs (medians). Prepass + forward
against forward alone:

| | RTX 4060 Ti, off → on | Radeon iGPU, off → on |
|---|---:|---:|
| Sponza, textured (BC) | 283 → 63 + 232 = 295 | 6,890 → 935 + 5,180 = 6,115 (−11%) |
| Sponza, factors only | 165 → 63 + 131 = 194 | 3,616 → 757 + 2,807 = 3,564 |
| FlightHelmet, textured | 61 → 23 + 42 = 65 | 1,782 → 470 + 1,342 = 1,812 |
| cubes 1k, city blocks | 84 → 15 + 63 = 78 | 2,036 → 286 + 1,327 = 1,613 (−21%) |
| cubes 10k, city blocks | 98 → 22 + 68 = 90 | 2,258 → 754 + 1,381 = 2,135 |
| cubes 10k, inside the field | 65 → 21 + 51 = 72 | 1,277 → 573 + 1,114 = 1,687 |
| cubes 10k, overview | 52 → 27 + 47 = 74 | 1,316 → 862 + 1,274 = 2,136 |
| cubes 100k, overview | 269 → 261 + 269 = 530 | – |

- **It pays** where expensive shading hides behind nearer geometry and vertices are
  few: a textured interior on the iGPU, city blocks.
- **It costs** a second pass over the vertices, as much as the shadow pass. Dense
  geometry roughly doubles its vertex work.
- **On the RTX, front-to-back order has already removed most overdraw.** The prepass
  saves 51 µs of Sponza's shading and costs 63.
- **Correctness:**
  - with and without the prepass the images are identical, pixel for pixel, on both GPUs:
    `tests/culling.rs` (hundreds of random objects) and `tests/materials.rs` (masked,
    double-sided and mirrored geometry in sunlight);
  - 0 validation errors under synchronization validation.

## Decision

- **`ForwardRenderer::set_depth_prepass(bool)`, off by default.**
- **With the prepass:**
  - without occlusion culling: "depth prepass", then "forward" with depth read-only and
    tested for equality;
  - with occlusion culling: "depth prepass early", the pyramid, "cull late", "depth
    prepass late", then one "forward" pass shading both phases' lists.
- **Pipelines:**
  - `depthVertex` (positions only) and `depthVertexMasked` (alpha test);
  - one equal-depth shading pipeline for every state;
  - cull mode and front face stay dynamic (ADR 0040).
- **Clip positions** come from `clip_position` in every pass that tests depth for
  equality (`precise`, products written out).
- `draw_bench` and `scene_bench` take `--prepass` and report its time.

## Consequences

- **Games with interiors and expensive materials** turn it on per renderer; the dev
  guide says when.
- **Revisit the default** when shading gets heavier (clustered lights, ambient occlusion,
  image-based lighting) or when meshlet culling cuts the vertex cost of a second pass.
- **Screen-space effects that need depth before shading** (ambient occlusion applied to
  the ambient term) require the prepass. Effects after shading (TAA, post-processing)
  use the forward pass's depth either way.
