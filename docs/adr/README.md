# Architecture Decision Records

Every architectural decision (new subsystem, dependency change, data format, public API,
threading model, build configuration) is recorded here as `NNNN-short-title.md`
(CLAUDE.md §3). Decisions change only through a new ADR that supersedes the old one.

## Index

| # | Title | Status |
|---|---|---|
| [0001](0001-workspace-and-build.md) | Workspace layout, toolchain and build profiles | Accepted |
| [0002](0002-job-system.md) | Job system | Accepted |
| [0003](0003-profiling.md) | Profiling instrumentation (Tracy) | Accepted |
| [0004](0004-platform-bindings-and-timing.md) | Platform bindings, timers and thread placement | Accepted |
| [0005](0005-window-and-input-threads.md) | Window thread and raw input thread | Accepted |
| [0006](0006-vulkan-baseline-and-device.md) | Vulkan baseline, bindings and device selection | Accepted |
| [0007](0007-presentation-and-frames.md) | Presentation and frames in flight (addendum 2026-10-10: a frame for a monitor on another GPU is drawn apart and copied) | Accepted |
| [0008](0008-shader-compilation.md) | Shader compilation: in-process Slang with a content-addressed cache | Accepted |
| [0009](0009-shader-hot-reload.md) | Shader hot reload and "Compile All Shaders" | Accepted |
| [0010](0010-gpu-memory-allocator.md) | GPU memory allocator | Accepted |
| [0011](0011-render-graph.md) | Render graph (addendum 2026-10-10: a pass's GPU time is its own on its queue, not the previous pass's tail) | Accepted |
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
| [0043](0043-motion-vectors-and-taau.md) | Motion vectors, projection jitter and the engine's own TAAU (addenda 2026-10-10: the depth prepass writes the motion vectors when it runs, the shading after it draws without; the Catmull-Rom history reads its texels one by one) | Accepted |
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
| [0058](0058-ambient-occlusion.md) | Ambient occlusion: GTAO from the depth prepass, in XeGTAO's structure | Accepted |
| [0059](0059-subsurface-scattering.md) | Subsurface scattering: a sum-of-Gaussians profile, a forward subsurface pass and a disk-sampled scatter pass | Accepted |
| [0060](0060-fsr-3-1-upscaling.md) | FSR 3.1 upscaling beside the engine's TAAU, built from source with our own shader generator; a texture LOD bias for both; frame generation later | Accepted |
| [0061](0061-logging-crash-reports-gpu-faults.md) | Failure visibility: a lock-free log behind the `log` facade, out-of-process crash reports, GPU breadcrumbs and `VK_EXT_device_fault` | Accepted |
| [0062](0062-golden-images-and-perf-gate.md) | Golden images compared through FLIP, and a per-pass GPU time gate (addendum 2026-10-10: runs a round at a time, a scene with a slower pass measured again, 100 frames per run, a short wait for a quiet CPU) | Accepted |
| [0063](0063-dlss-through-streamline.md) | DLSS Super Resolution through Streamline with manual hooking, from user-supplied DLLs; one `Upscaler` interface over TAAU, FSR and DLSS | Accepted |
| [0064](0064-gpu-scene-extraction-runtime.md) | Persistent GPU scene (stable slots, only changes uploaded: scatter or full copy with swapped buffers), extraction from the ECS by change detection and removal logs, incremental transform propagation, `engine_runtime` frame loop with the camera per rendered frame | Accepted |
| [0065](0065-async-compute.md) | Async compute in the render graph: opt-in passes scheduled by the graph across two queues, on by default where measured faster (NVIDIA) | Accepted |
| [0066](0066-physics-queries-characters-rollback.md) | Physics for games: shape casts and overlaps, Jolt's `CharacterVirtual` behind a backend-agnostic interface, kinematic targets, rollback snapshots into a reusable buffer, ECS sync with interpolation; 2D queries and Box2D's mover | Accepted |
| [0067](0067-no-camera-motion-blur-in-gameplay.md) | No camera motion blur in gameplay: cinematic views only, by license and by API | Accepted |
| [0068](0068-skeletal-animation-and-skinning.md) | Skeletal animation and skinning: an ozz-style runtime, ACL-style compressed clips in Rust, pre-skinning in compute with a depth-equal motion pass | Accepted |
| [0069](0069-sky-atmosphere-and-fog.md) | Physically based sky and atmosphere (Hillaire 2020): tables in compute passes, the sun's color and the image-based light from them, aerial perspective and exponential height fog in one full-screen pass | Accepted |
| [0070](0070-network-transport.md) | Network transport: own sans-I/O UDP endpoint with netcode-style connect tokens, AES-256-GCM through CNG, reliable-stream and unreliable channels with delivery notification, no I/O thread; NAT traversal UPnP → PCP/NAT-PMP → STUN hole punching → relay | Accepted |
| [0071](0071-local-light-and-contact-shadows.md) | Shadows of point and spot lights in a cached atlas (screen-size resolution, redrawn on change within a budget, GPU-driven per view, separate shading entry points) and Bend Studio's screen-space contact shadows of the sun | Accepted |
| [0072](0072-frame-renderer.md) | The frame renderer: one object composes a view's frame (scene passes, temporal antialiasing or upscaling, bloom, exposure, tone mapping), per-view state, the engine's exposure for FSR and DLSS, camera entities, measured defaults per device class | Accepted |
| [0073](0073-reflection-probes-and-screen-space-reflections.md) | Local reflections: probes captured by the forward renderer and prefiltered into a cube array, selected by screen cells and z-bins with parallax correction; Hi-Z stochastic screen-space reflections with spatial and temporal filters; one composite after the opaque surfaces, nothing in other views' shading | Accepted |
| [0074](0074-replication-prediction-lag-compensation.md) | Replication, prediction and lag compensation: reflected components in per-tick quantized frames, per-entity deltas against acknowledged (and extrapolated) states under a priority accumulator and byte budget, resend on loss notification; client prediction with the game's systems and rollback of predicted entities; time dilation; adaptive interpolation; a ring of hit capsules for lag compensation | Accepted |
| [0075](0075-antialiasing-and-scaling-modes.md) | Every antialiasing and scaling mode as three parts (raster samples, how the render size reaches the output, a filter after tone mapping), validated per GPU: MSAA 2/4/8 with alpha to coverage and every screen-space effect, FXAA, MLAA, SMAA 1x/S2x/T2x/4x, nearest, bilinear, integer and FSR 1 scaling; nothing costs a view that does not use it (addendum 2026-10-10: Catmull-Rom and Lanczos-3 scaling with an anti-ringing clamp, texels read one by one) | Accepted |
| [0076](0076-gpu-memory-budget-and-streaming.md) | A GPU memory budget owner (the OS budget, a share for the game, grants class by class), texture levels from a separate low-resolution feedback pass within it (images reallocated by copy), mesh detail levels stored and streamed as vertex prefixes, pack version 3, `engine_streaming` | Accepted |
| [0077](0077-faster-checks.md) | Faster checks: one workspace build per check, tests under cargo-nextest filtered per step (GPU crates on both GPUs, Tracy only for allocation tests, in its own target directory), doc tests and formatting for what the branch changed, and a workspace-hack crate that unifies dependency features so builds of a few packages reuse the workspace's | Accepted |
| [0078](0078-mip-fade.md) | Mip fade: streamed texture levels sharpen in over a short fade (the view's fractional minimum level, `VK_EXT_image_view_min_lod`; no shader change) | Accepted |
| [0079](0079-pipelines-on-first-use.md) | Shading variants described up front and made on first use or by the device's warm-up thread, whichever comes first (`LazyPipeline`, a layout kept alive by what may still be made with it); start-up phases overlapped (the device beside the window, the swapchain beside the renderers and the scene) | Accepted |
| [0080](0080-gpu-radix-sort.md) | GPU radix sort: reduce-then-scan over 4-bit digits (FidelityFX Parallel Sort's structure; no decoupled look-back, which NVIDIA patents), keys with a payload, counts written by the GPU, subgroup arithmetic required | Accepted |
| [0081](0081-gpu-particles.md) | GPU particles: a pool with a dead list and two alive lists, counts and indirect arguments on the GPU only, emitters described per frame, deterministic random numbers, sorted by depth (ADR 0080), drawn soft and premultiplied in the frame renderer's hook | Accepted |
| [0082](0082-decals.md) | Decals: a decal buffer before lighting (a compute pass after the depth prepass over the tiles some decal reaches, binned by tile and z-bin bitmasks, writes albedo, a normal change, roughness and metalness), blended by the shading's decal variants; not on transparent surfaces; measured against clustered decals in the shading (worse on the iGPU) | Accepted |
| [0083](0083-hdri-environments.md) | HDRI environments: `.hdr` panoramas kept as RGBE and decoded on the GPU, a background cube of their own turned when sampled, the sun found and taken out into a directional light (lighting cube without its spike; shadows); cooked into BC6H cubes for the DDC and packs (version 4); OpenEXR through the `exr` crate | Accepted |
| [0084](0084-global-illumination.md) | Global illumination: our own probe GI traced with ray queries (a probe volume converged at load, then a ray budget per frame; acceleration structures), held to cost less than AMD's Brixelizer GI, which is built as the reference and the fallback | Accepted |
| [0085](0085-visibility-buffer-and-meshlets.md) | Visibility buffer and meshlets: meshlets in the forward renderer first (cook-time clusters culled per view, compacted indices or mesh shaders); the visibility buffer decided with custom materials (item 3c) | Accepted |
| [0086](0086-global-allocator.md) | The global allocator: mimalloc 2.3.2 (`engine_core::alloc::EngineAllocator`) for the engine's executables (3.3–4.7× the system heap on allocation-heavy work, 1.5–1.6× on imports and pack loads), its memory given back by the job pool's idle workers and `ThreadPool::release_memory`; a counting layer in debug builds | Accepted |
| [0087](0087-acceleration-structures.md) | Acceleration structures for ray queries, built from the GPU scene: `engine_rhi` enables `VK_KHR_ray_query` and `VK_KHR_acceleration_structure`; a bottom level per mesh (fast trace, compacted), a top level of the GPU scene's slots written by a compute pass and rebuilt (fast trace) in frames where something changed; shaders reach it by address (addendum 2026-10-11: refitted between rebuilds, a rebuild after 8 refits; bottom-level builds under a per-frame budget) | Accepted |
| [0088](0088-probe-gi-volume.md) | Probe GI, phase 1: a volume of probes (octahedral irradiance and distance moments) traced with ray queries and converged at load, relocated and classified; read through a resolve pass at reduced resolution (2 × 2 pixels per sample on discrete GPUs, 4 × 4 on integrated ones) upsampled by distance in 34 GI twins of the shading entry points; Sponza at 1080p costs 0.12 ms per frame on the RTX, 1.8 ms on the iGPU | Accepted |

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
