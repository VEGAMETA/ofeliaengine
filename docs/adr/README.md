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
| [0011](0011-render-graph.md) | Render graph (addenda 2026-10-10: a pass's GPU time is its own on its queue, not the previous pass's tail; 2026-10-15: transient heaps the frame slots share, and the placements and objects of recent frame shapes kept) | Accepted |
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
| [0073](0073-reflection-probes-and-screen-space-reflections.md) | Local reflections: probes captured by the forward renderer and prefiltered into a cube array, selected by screen cells and z-bins with parallax correction; Hi-Z stochastic screen-space reflections with spatial and temporal filters; one composite after the opaque surfaces, nothing in other views' shading; addendum: the probe captured longest ago first, the scene's changes capture a probe at most every 30 frames | Accepted |
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
| [0089](0089-probe-gi-ray-budget.md) | Probe GI, phase 2: after convergence every frame traces windows of probes within a ray budget (131,072 rays on discrete GPUs, 8,192 on integrated ones), half in a box around the camera, half in turn over the volume, blended with a hysteresis of 0.9; Sponza 0.2 / 1.0 ms a frame on the RTX / iGPU | Accepted |
| [0090](0090-brixelizer-gi-reference.md) | Brixelizer GI built from the FidelityFX SDK as probe GI's reference (ADR 0084's bar): the SDK as one library for every effect (`engine_ffx`, with three fixes for AMD's compiler), a screen GI hook in the forward renderer, inputs computed for it, the GI waiting for the distance field; measured on both GPUs: probe GI meets the bar (4.5 / 14 times less GPU time, 29 / 18 times less memory, a physical bounce where Brixelizer GI gives a fifth) | Accepted |
| [0091](0091-frame-generation.md) | Frame generation: FSR 3.1 frame interpolation and optical flow built from the SDK and presented by the engine (a paced present thread with its own queue, no copy and no thread with FG off), DLSS FG through Streamline; the UI handed to the frame renderer as a layer and drawn on every presented frame, world-space and camera-attached widgets with that frame's camera; off by default | Accepted |
| [0092](0092-custom-materials.md) | Custom materials: Slang types implementing `IMaterial` that write a surface the engine lights, specialized into the passes that draw them (the built-in glTF material through the same interface, identical SPIR-V); only the (material, view features) pairs drawn are compiled and made; first sight drawn plain with the built-in shading (ubershaders tried); shading models lit, unlit, clear coat, cloth, hair, toon | Accepted |
| [0093](0093-cached-shadows-and-virtual-shadow-maps.md) | Cached shadows and virtual shadow maps: the sun's cascades kept across frames (wrap-around layers, only exposed strips and moved casters' regions drawn), then virtual pages for the sun (3a: the pages' machinery; 3b: drawn into and read by the shading, opt-in; 3c: draws clipped to dirty pages, a cheaper visibility, moved casters' pages only); one cache, no static/dynamic pair merged by a mask (patent) | Accepted |
| [0094](0094-color-grading.md) | Color grading: white balance, channel mixer, shadows / mid-tones / highlights, the ASC CDL and contrast in ACEScct, vibrance and saturation, in Filament's order; the grading and the tone curve in one 48³ LUT built on the GPU when either changes (with PBR Neutral the grading alone, its curve after it), read once per pixel by graded twins of the tone mapper's entry points; the plain ones unchanged | Accepted |
| [0095](0095-depth-of-field.md) | Depth of field: AMD FidelityFX DOF 1.1's tiled gather ported (near and far fields, ring buckets, composite), after the upscalers and before bloom; inverse depth instead of signed circles of confusion (patent), the near field from a radius of one pixel, no ring merging; a thin lens from the view's field of view, a sensor height, an f-number and a focus distance; off by default | Accepted |
| [0096](0096-motion-blur.md) | Motion blur: Guertin, McGuire and Nowrouzezahrai's reconstruction filter (tile max, neighbour max, a gather in two directions with direction-aware weights), on cinematic views only (`ViewKind::Cinematic { motion_blur }`, ADR 0067: a gameplay view cannot ask for it), after depth of field and before bloom | Accepted |
| [0097](0097-color-vision.md) | Color vision deficiencies simulated (the developer's check) and corrected (the player's filter): Machado's matrices for protans and deutans, Brettel's half-planes for tritans, daltonization, held by the graded tone mapper's LUT after the curve; nothing while off | Accepted |
| [0098](0098-texture-anisotropy.md) | Anisotropic filtering of material textures as a player setting (off, 2×, 4×, 8×, 16× by default), held by the material samplers like the LOD bias: nothing in the shaders | Accepted |
| [0099](0099-player-crate.md) | The player crate (`engine_app`) owns a game's wiring, in four steps; step 4: features per view, reflection probes and the GI volume as components (made with their first entities), screen-space reflections as a setting, an offscreen player, golden and perf gate shots of a game through it, the stress scene a game on it (the subsystem benches stay below it); step 3: lights (`PointLight`, `SpotLight`, `Sun`) and skinned meshes as components, a light keeping its slot while it lives; step 2: `Player` and the `Game` hooks, latency control on by default (RTX 7.90 → 5.66 ms, Radeon 69.3 → 52.3 ms to the screen), no allocation in a frame; step 1: windows fit into the work area, borderless full screen stating "disallowed" by default (NVIDIA flips it, 7.9 against 9.3 ms to the screen), exclusive full screen offered (`VK_EXT_full_screen_exclusive`, made again from nothing when refused) | Accepted |
| [0100](0100-video-playback.md) | Video playback: AV1 in WebM decoded by the GPU's video engine (Vulkan Video on an optional decode queue; both reference GPUs decode AV1), rav1d where that is missing and as the tests' bit-exact reference, our own WebM reader and AV1 header parser, a compute pass from YUV to an sRGB texture, the Opus sound as the clock, `VideoPlayer` in a new crate `engine_video` | Accepted |
| [0101](0101-mesh-colliders.md) | Colliders made from meshes, cooked into Jolt's binary form (restored at memory speed: Intel Sponza 3.9 s → 5.5 ms): glTF physics drafts and Unreal's collision names first, level meshes as triangles by default, props a fitted primitive or hull, a Rust decomposition after CoACD's algorithm (8c-3), any scale without errors (shapes Jolt cannot scale rebuilt, shears baked), colliders as the engine's editable data; `engine_collision`, pack version 6, `ScenePhysics`; the rules file `<scene>.colliders.ron`, hits' surfaces on demand, layers by name (8c-2b) | Accepted |
| [0102](0102-localization.md) | Localization: Fluent's syntax (`.ftl` per locale and module) cooked into a strings pack, our own resolver formatting into the caller's buffer without allocating, ICU4X's plural rules, numbers and dates (0.2 MB, 1 MB with dates), locale fallback and Windows' preferred languages, hot reload, pseudo-locales, UAX #9 in `engine_text` (`unicode-bidi`), font stacks per language, `ofeliaengine loc check`; `engine_loc` | Accepted |
| [0103](0103-navigation.md) | Navigation: Recast and Detour vendored (Zlib, `cc`, 64-bit polygon references) in `engine_nav`; the cook bakes the level geometry (static colliders as placed, primitives as their hulls) in parallel tiles by default (Sponza 6 tiles, 24 KB, 0.03 s; Intel Sponza 34 KB, 0.21 s), pack version 8, `Player::load_navigation` as the world's `NavMesh` resource; paths in microseconds without allocating; crowds, obstacles and off-mesh links later | Accepted |
| [0104](0104-save-games.md) | Save games: everything but what is marked `skip` (reflected components and resources, physics, the clock, random generators, animation), copied in the frame (10,000 entities: 0.023 ms) and encoded, compressed and swapped in atomically behind it; text written by development builds and a binary of named fields by release ones (12.0 against 1.3 ms to write), both always read; quicksave, quickload, slots and autosave in one call each, `before_save` and `after_load` hooks; integrity by default, signing or encryption as a game's setting | Accepted |
| [0105](0105-sound-in-the-player.md) | Sound in the player: `AudioSource` (as it is) and `AudioSource3D` (in the world) with `AudioListener` and `ReverbZone`, reflected and saved with each sound's position (a load resumes it there); one mixer for sources and videos, its device opened with the first sound; files from a live folder while developing and a sound pack in a release (pack version 10, streamed in place); walls muffling 3D sounds and the listener's room reverb from rays through the game's physics world, zones over it; 1,000 moving sources 17 µs a frame, 51 µs with the acoustics | Accepted |
| [0106](0106-networking-in-the-player.md) | Networking in the player: with `PlayerSettings::network` (`NetConfig::new::<Input>(schema)`) the player runs the session the game asks for through the world's `Network` resource (host, invite as text, join, leave, the host's pawn, pawns given, events, one-shot events as bytes tagged with their type); hosting, the server around the game's ticks with the router's port mapped in the background; joined, the client runs the frame's ticks (`Runtime::frame_driven`: predictions and re-simulations through `Game::tick`, one extraction, shown as it is); `Game::net_input` before every host tick and joined frame | Accepted |
| [0107](0107-editor.md) | The editor: a library linked into the game's build behind its `editor` feature (the owner's choice; a separate process later), driving the player's machinery; Play on a copy of the world with "Keep changes" as one undo step; every action a command feeding the palette, shortcuts and the automation interface; autosave and sessions in `.ofelia/`, rebuild and restart under a second; a built-in code editor on Luau's analysis library and a debugger core shared with VS Code (DAP); phases E1 to E14, tools in the owner's order (materials, sound, animation, 2D, then the rest) | Accepted |
| [0108](0108-scene-format-2.md) | Scene format 2: the saves' value grammar (strings, lists, maps, optional values), `Name`; assets by id and path (the owner's mix: ids inside our own files, a `.meta` beside foreign ones, `asset("path#part", id)` references, the local database only a cache); prefab instances with overrides keyed by the prefab's entity ids (proposed: any scene is a prefab) | Accepted in part |
| [0109](0109-viewport-picking.md) | Viewport picking and the selection's outline: the view's culled commands drawn again into a 32 × 32 square under the pointer or the box (slot ids, read back through a ring, as texture feedback); hidden objects and lists from candidate slots drawn without the depth test (`atomicMin` per slot); the outline from a selected-only mask at the output size and a 2 px edge pass, hidden parts at 35 %; only editor views pay | Accepted |
| [0110](0110-blockout-brushes.md) | Blockout: convex brushes held as integer planes (normals up to 2^22 through a 1 mm grid, every side exact in `i128`), evaluated in Godot's order over convex pieces kept per additive brush and per face tile, so a door dragged among 2,001 brushes redoes 0.58 ms; groups as `CSGCombiner3D`; a mesh per brush with a grid material type, the pieces as hulls, the visible surface for navigation; `engine_brush` | Proposed |

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
