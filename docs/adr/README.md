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
| [0017](0017-ecs.md) | ECS: own archetype ECS with growing chunks, canonical order, snapshots | Accepted |
| [0018](0018-physics-interface-and-jolt.md) | Physics interface (P0) and the Jolt reference backend (P1) | Accepted |
| [0019](0019-networking-model.md) | Networking model: authoritative ticks, prediction, lag compensation | Accepted |
| [0020](0020-engine-license.md) | Engine license: MIT as a placeholder; the source-available terms recorded for the final decision | Accepted (placeholder) |
| [0021](0021-input-actions.md) | Input actions: named actions, contexts, tick-accurate state | Accepted |
| [0022](0022-reflection.md) | Reflection: static field tables with offsets | Accepted |
| [0023](0023-latency.md) | Latency: Reflex, Anti-Lag 2 and a generic path behind one interface | Accepted |
| [0024](0024-audio-output-and-mixer.md) | Audio output and mixer: WASAPI low latency, lock-free commands, compiled bus schedule | Accepted |
| [0025](0025-text.md) | Text: own layout on HarfRust and skrifa, bitmap glyph atlas | Accepted |
| [0026](0026-system-scheduler.md) | ECS system scheduler: insertion order, access-derived levels, job system | Accepted |
| [0027](0027-transforms.md) | Transforms: local and world components, propagation from roots | Accepted |
| [0028](0028-physics-scope.md) | Physics scope: Jolt stays, own 3D solver deferred; 2D on Box2D v3 | Accepted |
| [0029](0029-scene-format.md) | Scene text format: line-oriented blocks, one field per line | Accepted |
| [0030](0030-ui-core.md) | UI core: immediate-mode building, retained state, full-tree layout | Accepted |
| [0031](0031-undo.md) | Undo: transactions of entity snapshots, identified by scene ids | Accepted |
| [0032](0032-scripting.md) | Scripting: Luau for game logic, Rust modules for hot loops, WASM later | Accepted (API provisional) |
| [0033](0033-gltf-import.md) | glTF 2.0 import into memory with the `gltf` crate | Accepted |
| [0034](0034-gpu-culling-and-indirect-draws.md) | GPU frustum culling and indirect instanced draws | Accepted |
| [0035](0035-occlusion-culling.md) | Two-phase occlusion culling (hierarchical depth) | Accepted |
| [0036](0036-fonts-and-icons.md) | Default fonts and icons; OFL-1.1 and CC BY 4.0 for font and icon assets | Accepted |
| [0037](0037-checks-on-the-reference-machine.md) | Checks on the reference machine before merging; GitHub Actions on demand | Accepted |
| [0038](0038-hdr-output.md) | HDR output: scRGB by default, following Windows at run time | Accepted |
| [0039](0039-geometry-pool.md) | Geometry pool: shared vertex and index buffers, 28-byte vertices | Accepted |
| [0040](0040-materials-and-textures.md) | Materials and textures: a bindless material table, filtered mip chains | Accepted |
| [0041](0041-texture-compression.md) | Texture compression: BC7 and BC5 on the CPU, a derived data cache | Accepted |
| [0042](0042-depth-prepass.md) | Depth prepass: optional, off by default | Accepted |
| [0043](0043-motion-vectors-and-taau.md) | Motion vectors, projection jitter and the engine's own TAAU | Accepted |
| [0044](0044-cascaded-shadow-maps.md) | Cascaded shadow maps: stable cascades in an atlas, casters culled per cascade on the GPU | Accepted |
| [0045](0045-local-lights.md) | Local lights: z-bins and tile bitmasks | Accepted |
| [0046](0046-image-based-lighting.md) | Image-based lighting from an environment | Accepted |
| [0047](0047-bloom-and-automatic-exposure.md) | Bloom and automatic exposure | Accepted |
| [0048](0048-tone-curve.md) | The tone curve: Uchimura's by default, PBR Neutral as an option | Accepted |
| [0049](0049-reverb.md) | Reverb: diffusion into an 8-line feedback delay network | Accepted |
| [0050](0050-spatial-audio.md) | Spatial audio: emitters and a listener, computed on the audio thread per render call | Accepted |
| [0051](0051-compressed-audio-and-streaming.md) | Compressed audio and streaming: lewton, vendored libopus, claxon; an SPSC ring per stream | Accepted |
| [0052](0052-compute-passes.md) | Compute passes: shaders typed by their push constants, checked against SPIR-V | Accepted |
| [0053](0053-mesh-lods.md) | Mesh detail levels: meshoptimizer at import, chosen per instance by the culling pass | Accepted |
| [0054](0054-transparency.md) | Transparency: sorted blended instances in a forward pass, a reactive mask for TAAU | Accepted |
| [0055](0055-pipeline-cache-and-warm-up.md) | Pipeline cache and warm-up: a validated VkPipelineCache on disk, cache lookups first, compiles in parallel | Accepted |
| [0056](0056-cooked-packs-and-streaming-reads.md) | Cooked packs and streaming reads: our own container, an I/O ring, zstd per entry | Accepted |
| [0057](0057-texture-streaming.md) | Texture streaming: reads straight into a host-cached staging ring, levels arriving smallest first, a streaming thread | Accepted |
| [0060](0060-fsr-3-1-upscaling.md) | FSR 3.1 upscaling beside the engine's TAAU, built from source with our own shader generator; a texture LOD bias for both; frame generation later | Accepted |
| [0058](0058-ambient-occlusion.md) | Ambient occlusion: GTAO from the depth prepass, in XeGTAO's structure | Accepted |
| [0062](0062-golden-images-and-perf-gate.md) | Golden images compared through FLIP, and a per-pass GPU time gate | Accepted |

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
