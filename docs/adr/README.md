# Architecture Decision Records

Every architectural decision (new subsystem, dependency change, data format, public API,
threading model, build configuration) is recorded here as `NNNN-short-title.md`
(CLAUDE.md §3). Decisions change only through a new ADR that supersedes the old one.

## Index

| # | Title | Status |
|---|---|---|
| [0001](0001-workspace-and-build.md) | Workspace layout, toolchain and build profiles | Proposed |
| [0002](0002-job-system.md) | Job system | Accepted |
| [0003](0003-profiling.md) | Profiling instrumentation (Tracy) | Accepted |
| [0004](0004-platform-bindings-and-timing.md) | Platform bindings, timers and thread placement | Accepted |
| [0005](0005-window-and-input-threads.md) | Window thread and raw input thread | Accepted |
| [0006](0006-vulkan-baseline-and-device.md) | Vulkan baseline, bindings and device selection | Accepted |
| [0007](0007-presentation-and-frames.md) | Presentation and frames in flight | Accepted |
| [0008](0008-shader-compilation.md) | Shader compilation: in-process Slang with a content-addressed cache | Accepted |
| [0009](0009-shader-hot-reload.md) | Shader hot reload and "Compile All Shaders" | Accepted |
| [0010](0010-gpu-memory-allocator.md) | GPU memory allocator | Accepted |
| [0011](0011-render-graph.md) | Render graph | Accepted |
| [0012](0012-math-library.md) | Math library | Accepted |
| [0013](0013-bindless-and-pipelines.md) | Bindless resources, pipelines and the first mesh path | Accepted |
| [0014](0014-pbr-and-shadows.md) | Forward PBR, directional shadows, per-frame GPU data | Accepted |
| [0015](0015-renderer-2d.md) | 2D renderer: bindless quad batching, SDF shapes, deterministic order | Accepted |
| [0016](0016-viewports.md) | Viewports: persistent textures, topological order, graph scopes, composition | Accepted |

## Template

```markdown
# NNNN. Title

- Status: Proposed | Accepted | Superseded by NNNN
- Date: YYYY-MM-DD

## Context
What problem are we solving, and what constraints apply (link CLAUDE.md sections,
research notes in docs/research).

## Options
At least two. For each: how it works, pros, cons.

## Measurements / arguments
Numbers with hardware, build profile and commit. If something could not be measured
yet, say so explicitly and say where and when it will be measured.

## Decision

## Consequences
What becomes easier, what becomes harder, what must be revisited and when.
```
