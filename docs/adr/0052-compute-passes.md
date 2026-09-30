# 0052. Compute passes: shaders typed by their push constants, checked against SPIR-V

- Status: Accepted
- Date: 2026-09-30
- Research: [docs/research/compute-passes.md](../research/compute-passes.md)
- Related: [ADR 0013](0013-bindless-and-pipelines.md) (bindless, push constants),
  [ADR 0011](0011-render-graph.md) (render graph)

## Context

Every compute pass in the engine repeated the same code:
- embedding the SPIR-V, converting it to words, creating the pipeline;
- a `Push` struct with a hand-written size assertion and an `unsafe` byte view;
- an `unsafe` helper binding the pipeline and the bindless set, pushing and dispatching;
- workgroup counts computed from a workgroup size copied by hand from the shader.

The push constant struct exists twice, in Slang and in Rust; a field added on one side
only made the shader read garbage, found (if at all) by a GPU test. Writing a new compute
pass took more ceremony than shader code.

## Options

1. **Keep the pattern, add a macro** that generates the boilerplate: shorter, but the two
   struct definitions still drift apart silently.
2. **Generate Rust structs from Slang reflection** at shader build time: one source of
   truth, but it needs the Slang reflection API in the build (the committed SPIR-V is
   built by a tool, not by cargo) and generated code in every crate with shaders.
3. **A typed pipeline checked against the SPIR-V at creation** (chosen):
   `ComputeShader<P>` reads the workgroup size and the push constant block's size from the
   module (a small SPIR-V reader in `engine_rhi`), refuses a `P` smaller than the block,
   and a graph pass type does the binding, pushing and dispatching.

## Decision

- **`engine_rhi::spirv::reflect`**: workgroup size (`LocalSize`, `LocalSizeId`,
  `WorkgroupSize`) and push constant block size (member offsets and sizes, array and
  matrix strides, buffer device addresses) from SPIR-V words. It reads all 34 committed
  modules; their sizes equal the Rust structs'. `RhiError::ShaderInterface` reports a
  mismatch.
- **`engine_rhi::Pipeline::compute_specialized`** (from ADR 0043's amendment) stays the
  primitive underneath.
- **`engine_render::compute`**:
  - `ComputeShader<P: bytemuck::Pod>::new/specialized`: the pipeline; fails if the shader
    reads more than `size_of::<P>()` bytes; a `P` over 128 bytes does not compile;
  - `FrameGraph::add_compute(name, bindless)` → `ComputePass` (`read`, `write`,
    `side_effects`) → `dispatch(shader, Dispatch, push)` for one dispatch, or
    `record(|rec| ..)` for several (`rec.dispatch`, `rec.barrier` between dependent ones);
  - `Dispatch::Threads` rounds up by the shader's workgroup size, `Dispatch::Groups`
    counts workgroups;
  - the push closure returns `Option<P>`, so a missing bindless index skips the dispatch
    with `?`.
- **Push structs derive `bytemuck::Pod`**: the derive refuses hidden padding, which
  replaces the hand-written size assertions and `unsafe` byte views.
- **Every engine compute pass uses it**: bloom, exposure, environment, depth pyramid,
  culling, light tiles, TAAU.

## Numbers

- Code: 425 lines removed and 193 added in the passes (plus the two new modules); the
  `unsafe` blocks of compute recording are gone (postfx 9 → 3, environment 8 → 5, hiz
  3 → 1, forward 16 → 13, TAAU 7 → 5; the rest create and free resources or record
  graphics).
- GPU cost unchanged (the same commands): Sponza 1080p on the RTX 4060 Ti, bloom
  112–122 µs, exposure 17–18 µs.

## Consequences

- A new compute pass is a shader, a `Pod` struct and a few lines in the graph; the common
  drift between the two struct definitions is caught when the pipeline is created.
- Only the block's total size is checked: two fields swapped keep the size and pass. A
  per-field check needs member names in the SPIR-V (debug info) or Slang reflection;
  revisit if it happens.
- Graphics pipelines still use the hand-written pattern; the same reader can check them
  when a graphics pass helper is written.
- Next: hot reload of the engine's own passes (the editor already reloads user shaders,
  ADR 0009).
