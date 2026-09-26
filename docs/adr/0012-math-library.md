# 0012. Math library

- Status: Accepted
- Date: 2026-09-26

## Context

The renderer needs vectors, matrices and quaternions now (cameras, transforms). Physics,
animation and gameplay need them next. CLAUDE.md §2 puts "math (SIMD)" in
`engine_core`. §15 says not to add a dependency for something written in an hour, but a
math library with SIMD, tests and conventions is not an hour's work.

## Options

1. **glam 0.33** (MIT OR Apache-2.0, no dependencies):
   - `Vec2/3/4`, `Vec3A` (16-byte aligned), `Mat3/Mat3A/Mat4`, `Quat`, `Affine3A`;
   - SSE2 SIMD on x86-64;
   - used by Bevy and most Rust game code;
   - actively maintained.
2. **ultraviolet 0.10:** strong at "wide" SoA types (`Vec3x8`) for SIMD over many
   elements, weaker for everyday scalar-style code, and a smaller ecosystem.
3. **nalgebra 0.35** (Apache-2.0): general linear algebra, generic dimensions. Slower to
   compile and heavier than game math needs.
4. **Our own types:** full control, but SIMD, tests and ergonomics would repeat glam's
   work.

## Measurements

`engine_core/benches/math.rs`, container (4 vCPU, **not reference**), release, target
`x86-64-v2`. glam against a straightforward scalar implementation (what our own types
would start as):

| Operation | glam ns | scalar ns | scalar / glam |
|---|---|---|---|
| Mat4 × Vec4 (1M points, streaming) | 2.43 | 2.51 | 1.03× |
| Mat4 × Mat4 (dependent chain) | 5.44 | 10.71 | 1.97× |
| Quat rotate Vec3 (1M points) | 3.02 | 3.14 | 1.04× |

- For streaming loops the compiler vectorizes the scalar code as well, or memory bandwidth
  limits both.
- For latency-bound matrix chains (scene graphs, skeletal animation), glam's SIMD is 2×
  faster.

## Decision

- `engine_core::math` re-exports glam 0.33's types, so every crate uses one version.
- **Conventions:**
  - right-handed world, +Y up;
  - column vectors, column-major storage (matches Slang/SPIR-V and push constants
    without transposes);
  - Vulkan clip-space specifics (depth 0..1, Y flip) live in the renderer's projection
    helpers.
- Wide SoA types for physics (AVX2 via runtime dispatch, §5) will be our own, in this
  module, when that work starts.

## Consequences

- One small dependency, with no transitive dependencies. It is in `THIRD_PARTY_NOTICES.md`.
- **Determinism for physics** (§8): glam's transcendental functions call the platform's
  `libm`. That is deterministic for one build on one CPU class, but not across compilers.
  Physics will use its own functions where determinism matters.
- The `fast-math` and `scalar-math` features stay off.
