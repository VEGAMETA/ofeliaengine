# Compute passes: less ceremony, fewer silent mismatches

Research for making compute shaders easier to write (the team's request after the
rendering work of ADRs 0043–0048). Decision: [ADR 0052](../adr/0052-compute-passes.md).

Checked on 2026-09-30. Engines are described from their public documentation; no code
was read or copied.

## What a compute pass cost before

Seven passes (bloom down/up, exposure histogram/average, environment, depth pyramid,
culling, light tiles, TAAU) each carried:

- the SPIR-V embedded, converted to words and made into a pipeline with an error mapping;
- a `#[repr(C)] Push` with `assert!(size_of::<Push>() == N)` and an `unsafe` function
  viewing it as bytes;
- an `unsafe` recording helper: bind the pipeline, bind the bindless set, push, dispatch;
- `div_ceil(8)` and similar, with the workgroup size copied from the shader.

The struct lives twice (Slang and Rust). Nothing checks that the two agree: a field added
on one side shifts the other side's reads.

## How others do it

- **Unreal Engine:** shader parameters are declared once in C++ macros
  (`SHADER_PARAMETER_STRUCT`), from which both the binding code and validation derive;
  `FComputeShaderUtils::AddPass` adds a render graph pass that sets parameters and
  dispatches `GetGroupCount(threads, groupSize)`.
- **wgpu / Bevy:** WGSL is reflected by naga; pipeline layouts come from the shader, and
  mismatched bindings are validation errors at pipeline creation.
- **Slang** has a reflection API (parameter layouts, `numthreads`) that tools can use to
  generate host-side structs.

Common thread: one source of truth for the interface, or a check at creation, and a
helper that turns "threads" into "workgroups".

## What we take

- The workgroup size and the push constant block are in the SPIR-V the engine already
  embeds; reading them needs only a small subset of the format (`OpExecutionMode`,
  types, member offsets and strides, the `PushConstant` variable).
- Tie the pipeline to its Rust push type (`ComputeShader<P>`), compare sizes at creation.
- `bytemuck::Pod` (already used by `engine_ecs` and `engine_assets`) replaces the byte
  views and refuses implicit padding at compile time.
- A graph pass type that binds, pushes and dispatches, with `Threads` and `Groups`, and a
  recorder for passes with several dependent dispatches (mip chains).

## Measured

- The reader agrees with every Rust push struct in the engine (34 modules: 11 compute,
  23 graphics; 40 to 112 bytes).
- After moving every compute pass to the helper, the GPU times are unchanged (same
  commands); 232 lines fewer in the passes.

## Open questions

- Per-field checks (names and offsets) need debug info in the SPIR-V or Slang reflection
  at build time.
- The same for graphics passes.
- Hot reload of engine passes.
